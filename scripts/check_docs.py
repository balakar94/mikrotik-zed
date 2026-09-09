#!/usr/bin/env python3
"""
Docs gate: hygiene + relative links/anchors + index reachability + volatile-literal ban.

Stdlib only. Offline: external http(s) links are syntax-checked, never fetched.

Checks:
  H1  No trailing whitespace, single trailing newline, no heading-level skips.
  H2  First heading in each file is `#` (single h1).
  L1  Every relative `[text](target)` resolves on disk; `#anchor` matches a
      slugged heading in the target file (GitHub slug algorithm).
  L2  No absolute `/docs/...` links (use relative).
  R1  Every `docs/*.md` (except allowlisted dirs/files) is reachable from
      `docs/index.md` via local links.
  V1  No pasted volatile literals (versions, 40-hex SHAs, menu counts,
      RouterOS snapshots) outside fenced code blocks — with allowlist for
      CHANGELOG/ADR history and explicit `<!-- volatile-ok -->` markers.

Usage:
  python3 scripts/check_docs.py [--docs DIR]

Exit codes:
  0  docs valid
  1  violations found
  2  IO or usage error
"""

import argparse
import pathlib
import re
import sys
import unicodedata

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
DEFAULT_DOCS = REPO_ROOT / "docs"

ALLOWLIST_DIRS = {"adr", "export-fixtures"}
ALLOWLIST_FILES = {"publishing-runbook.md"}

VOLATILE_RES = [
    re.compile(r"\b\d+\.\d+\.\d+\b"),  # pasted versions (x.y.z)
    re.compile(r"\b[0-9a-f]{40}\b"),  # pasted SHAs
    re.compile(r"\b\d{3,4}\s+menus\b"),  # pasted menu counts
    re.compile(r"RouterOS\s+v?7\.\d+"),  # pasted snapshots
]
# Caps/keys documented from lsp/src/caps.rs are allowed by construction.
CAPS_ALLOW = re.compile(r"(MAX_[A-Z_]+|MiB|KiB|RSC_LS_|MIKROTIK_|live\s+—|0!live_)")
FENCE_RE = re.compile(r"^\s*```")
HEADING_RE = re.compile(r"^(#{1,6})\s+(.+?)\s*(?:#+\s*)?$")
LINK_RE = re.compile(r"(?<!!)\[([^\]]*)\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
VOLATILE_OK_MARKER = "<!-- volatile-ok"


def slug(heading: str) -> str:
    """GitHub heading-anchor slug algorithm (stdlib)."""
    text = unicodedata.normalize("NFKD", heading).encode("ascii", "ignore").decode()
    text = text.lower()
    text = re.sub(r"[^a-z0-9 _-]", "", text)
    text = re.sub(r" +", "-", text)
    return text.strip("-")


def strip_fences(lines: list[str]) -> list[tuple[int, str]]:
    """Return (lineno, line) pairs outside fenced code blocks."""
    out: list[tuple[int, str]] = []
    in_fence = False
    for i, line in enumerate(lines, start=1):
        if FENCE_RE.match(line):
            in_fence = not in_fence
            continue
        if not in_fence:
            out.append((i, line))
    return out


def collect_headings(text: str) -> dict[str, int]:
    """Map slug -> heading level for all headings (including in fences is fine)."""
    headings: dict[str, int] = {}
    for line in text.splitlines():
        m = HEADING_RE.match(line)
        if m:
            headings[slug(m.group(2))] = len(m.group(1))
    return headings


def check_hygiene(path: pathlib.Path, lines: list[str], errs: list[str]) -> None:
    if not lines:
        errs.append(f"{path}: empty file")
        return
    for i, line in enumerate(lines, start=1):
        stripped = line.rstrip("\n")
        if stripped != stripped.rstrip(" \t"):
            errs.append(f"{path}:{i}: trailing whitespace (H1)")
    if not lines[-1].endswith("\n"):
        errs.append(f"{path}: missing trailing newline (H1)")
    if lines[-1].strip() == "":
        # Allow exactly one trailing newline; flag stacked blanks.
        if len(lines) > 1 and lines[-2].strip() == "":
            errs.append(f"{path}: multiple blank lines at EOF (H1)")
    # Heading levels: first heading must be h1, no level skips.
    prev_level = 0
    seen_h1 = False
    for i, line in strip_fences(lines):
        m = HEADING_RE.match(line.strip())
        if not m:
            continue
        level = len(m.group(1))
        if not seen_h1:
            if level != 1:
                errs.append(f"{path}:{i}: first heading must be `#` (H2)")
            seen_h1 = True
        else:
            if level > prev_level + 1:
                errs.append(f"{path}:{i}: heading level skipped {prev_level}→{level} (H1)")
        prev_level = level


def check_links(
    path: pathlib.Path,
    lines: list[str],
    headings: dict[pathlib.Path, dict[str, int]],
    errs: list[str],
) -> None:
    for i, line in strip_fences(lines):
        if VOLATILE_OK_MARKER in line:
            continue
        for m in LINK_RE.finditer(line):
            target = m.group(2).strip()
            if target.startswith(("http://", "https://", "mailto:")):
                continue  # offline: syntax only, never fetch
            if target.startswith("/"):
                errs.append(f"{path}:{i}: absolute link `{target}` — use relative (L2)")
                continue
            if target.startswith("#"):
                anchor = target[1:]
                local = headings.get(path, {})
                if anchor and anchor not in local:
                    errs.append(f"{path}:{i}: anchor `#{anchor}` not found in file (L1)")
                continue
            if "#" in target:
                file_part, anchor = target.split("#", 1)
            else:
                file_part, anchor = target, ""
            if not file_part:
                continue
            resolved = (path.parent / file_part).resolve()
            try:
                resolved.relative_to(REPO_ROOT)
            except ValueError:
                errs.append(f"{path}:{i}: link escapes repo `{target}` (L1)")
                continue
            if not resolved.exists():
                # Allow directory links (adr/, export-fixtures/) without index.
                if resolved.is_dir() or (REPO_ROOT / resolved).exists():
                    continue
                errs.append(f"{path}:{i}: link target not found `{target}` (L1)")
                continue
            if anchor and resolved.suffix == ".md":
                other = headings.get(resolved, collect_headings(resolved.read_text(encoding="utf-8")))
                if anchor not in other:
                    errs.append(f"{path}:{i}: anchor `#{anchor}` not found in `{file_part}` (L1)")


def check_volatile(path: pathlib.Path, lines: list[str], errs: list[str]) -> None:
    rel = path.relative_to(REPO_ROOT).as_posix()
    if rel.startswith("docs/adr/") or "CHANGELOG" in rel:
        return  # history is allowed to quote old values
    for i, line in strip_fences(lines):
        if VOLATILE_OK_MARKER in line or CAPS_ALLOW.search(line):
            continue
        # Skip inline code spans: volatile facts inside `backticks` are references.
        code_stripped = re.sub(r"`[^`]*`", "", line)
        for rx in VOLATILE_RES:
            m = rx.search(code_stripped)
            if m:
                errs.append(f"{path}:{i}: pasted volatile literal `{m.group(0)}` — link canonical file (V1)")
                break


def check_reachability(docs: pathlib.Path, headings: dict[pathlib.Path, dict[str, int]], errs: list[str]) -> None:
    index = docs / "index.md"
    if not index.exists():
        errs.append("docs/index.md: missing landing page (R1)")
        return
    # BFS from index over local .md links.
    seen: set[pathlib.Path] = {index.resolve()}
    frontier = [index.resolve()]
    while frontier:
        current = frontier.pop()
        try:
            text = current.read_text(encoding="utf-8")
        except OSError:
            continue
        for m in LINK_RE.finditer(text):
            target = m.group(2).strip().split("#")[0]
            if not target or target.startswith(("http://", "https://", "mailto:", "/", "#")):
                continue
            resolved = (current.parent / target).resolve()
            if resolved.suffix == ".md" and resolved.exists() and resolved not in seen:
                seen.add(resolved)
                frontier.append(resolved)
    for md in sorted(docs.glob("*.md")):
        resolved = md.resolve()
        if resolved in seen:
            continue
        rel = md.relative_to(REPO_ROOT).as_posix()
        if any(part in ALLOWLIST_DIRS for part in md.relative_to(docs).parts[:-1]):
            continue
        if md.name in ALLOWLIST_FILES:
            # Must still be linked from index explicitly.
            index_text = index.read_text(encoding="utf-8")
            if md.name not in index_text and md.stem not in index_text:
                errs.append(f"{rel}: not linked from docs/index.md (R1)")
            continue
        errs.append(f"{rel}: not reachable from docs/index.md (R1)")


def main() -> int:
    parser = argparse.ArgumentParser(description="Docs gate: hygiene, links, reachability, volatile ban.")
    parser.add_argument("--docs", default=str(DEFAULT_DOCS), help="docs directory")
    args = parser.parse_args()

    docs = pathlib.Path(args.docs)
    if not docs.is_dir():
        print(f"error: docs dir not found: {docs}", file=sys.stderr)
        return 2

    errs: list[str] = []
    files = sorted(p for p in docs.glob("*.md") if p.is_file())
    if not files:
        print("error: no docs/*.md files found", file=sys.stderr)
        return 2

    headings: dict[pathlib.Path, dict[str, int]] = {}
    cache: dict[pathlib.Path, list[str]] = {}
    for path in files:
        try:
            text = path.read_text(encoding="utf-8")
        except OSError as exc:
            errs.append(f"{path}: unreadable: {exc}")
            continue
        lines = text.splitlines(keepends=True)
        cache[path] = lines
        headings[path.resolve()] = collect_headings(text)
        check_hygiene(path, lines, errs)
        check_volatile(path, lines, errs)

    for path in files:
        if path in cache:
            check_links(path, cache[path], headings, errs)

    check_reachability(docs, headings, errs)

    print(f"docs-check: {len(files)} files, {len(errs)} problems")
    for err in errs:
        print(f"error: {err}")
    return 1 if errs else 0


if __name__ == "__main__":
    sys.exit(main())
