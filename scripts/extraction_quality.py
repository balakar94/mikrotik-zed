"""Source attribution for offline extraction.

The pipeline reads two upstream documents: `llms-full.txt` (the combined
corpus) and `llms.txt` (the index of page titles and official URLs). This
module turns the index into per-line page identity so every parsed table can
name the page it came from, and refuses to guess when identity is missing.
"""

import re
from pathlib import Path
from urllib.parse import urlsplit

INDEX_LINK = re.compile(r"^- \[([^\]]+)\]\((https://manual\.mikrotik\.com/docs/[^\s)]+)\)")
INLINE_PATH = re.compile(r"`(/[a-z0-9][a-z0-9/_-]*)`")


def load_source_index(path: Path) -> dict[str, str]:
    """Map unique index titles to official URLs; ambiguous titles stay absent."""
    if not path.exists():
        return {}
    candidates: dict[str, set[str]] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        match = INDEX_LINK.match(line)
        if match:
            title, url = match.groups()
            candidates.setdefault(title.strip().lstrip("/").casefold(), set()).add(url)
    return {title: next(iter(urls)) for title, urls in candidates.items() if len(urls) == 1}


def source_contexts(lines: list[str], index: dict[str, str]) -> list[dict]:
    """Identify indexed page headings, ignoring fenced examples.

    Upstream separates documents with horizontal rules. Only the first H2
    after a separator (or the preamble) may establish an indexed identity;
    an internal heading matching another page title cannot steal ownership.
    Without separators, unmatched documents keep the conservative fallback.
    """
    result: list[dict] = []
    current: dict = {}
    fence = ""
    awaiting_title = True
    for position, line in enumerate(lines):
        stripped = line.lstrip()
        if fence:
            result.append({**current, "code": True})
            if re.fullmatch(re.escape(fence[0]) + "{" + str(len(fence)) + r",}\s*", stripped):
                fence = ""
            continue
        marker = re.match(r"(`{3,}|~{3,})", stripped)
        if marker:
            fence = marker.group(1)
            result.append({**current, "code": True})
            continue
        if stripped.strip() == "---":
            # Rules inside a page often separate prose or images. A source
            # boundary must lead directly to an H2, not to ordinary content.
            following = position + 1
            while following < len(lines) and not lines[following].strip():
                following += 1
            if following < len(lines) and lines[following].startswith("## "):
                current = {}
                awaiting_title = True
        heading = re.match(r"^## (.+?)\s*$", line)
        if heading and awaiting_title:
            awaiting_title = False
            title = heading.group(1).strip().rstrip(".")
            url = index.get(title.lstrip("/").casefold())
            if url:
                current = {"title": title, "url": url, "start_line": len(result) + 1}
        result.append(current)
    return result


def source_menu_path(url: str) -> str | None:
    """Return the CLI path only for an official CLI-reference page URL."""
    parsed = urlsplit(url)
    prefix = "/docs/cli-reference/"
    if parsed.hostname != "manual.mikrotik.com" or not parsed.path.startswith(prefix):
        return None
    path = parsed.path[len(prefix):].removesuffix(".md").rstrip("/")
    return "/" + path if re.fullmatch(r"[a-z0-9][a-z0-9/_-]*", path) else None
