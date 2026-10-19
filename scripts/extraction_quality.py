"""Source attribution and deterministic coverage reports for offline extraction.

The pipeline reads two upstream documents: `llms-full.txt` (the combined
corpus) and `llms.txt` (the index of page titles and official URLs). The
attribution helpers below turn the index into per-line page identity, so every
parsed table names the page it came from and refuses to guess when identity is
missing; the report helpers turn one run into a snapshot that a later run can
be compared against, so provenance, coverage or properties cannot disappear
unnoticed.
"""

import hashlib
import json
import re
from pathlib import Path
from urllib.parse import urlsplit

INDEX_LINK = re.compile(r"^- \[([^\]]+)\]\((https://manual\.mikrotik\.com/docs/[^\s)]+)\)")
INLINE_PATH = re.compile(r"`(/[a-z0-9][a-z0-9/_-]*)`")

# Report schema published to CI and to local baselines. A version bump means
# older baselines must be regenerated, never silently reinterpreted.
REPORT_SCHEMA_VERSION = 2
COVERAGE_VALUES = ("reference", "partial", "unknown")
_COVERAGE_RANK = {"reference": 2, "partial": 1, "unknown": 0}


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


def make_report(menus: list[dict], tables: list[dict], stats: dict, source: Path) -> dict:
    """Build a stable report; line numbers are evidence, never identity keys.

    Each menu records its property names, its coverage marker, its source URL
    and how many properties carry no evidence at all, so a later run can be
    compared per menu instead of through a dataset-wide aggregate.
    """
    index_path = source.with_name("llms.txt")
    described: dict[str, dict] = {}
    for menu in menus:
        menu_url = menu.get("source_url", "")
        entries = [entry for section in ("arguments", "flags", "read_only") for entry in menu.get(section, [])]
        described[menu["path"]] = {
            "properties": sorted({entry["name"] for entry in entries}),
            "coverage": menu.get("coverage", "unknown"),
            "source_url": menu_url,
            "unattributed_properties": sum(
                1
                for entry in entries
                if not (entry.get("source_url") or (menu_url if entry.get("source_kind") == "cli-reference" else ""))
            ),
        }
    return {
        "schema_version": REPORT_SCHEMA_VERSION,
        "source_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
        "index_sha256": hashlib.sha256(index_path.read_bytes()).hexdigest() if index_path.exists() else None,
        "stats": stats,
        "menus": described,
        "tables": tables,
    }


def report_regressions(previous: dict, current: dict) -> list[str]:
    """Compare menus, provenance, coverage and unresolved tables across runs.

    Aggregates are not enough: a dataset-wide URL total can hold while single
    menus lose their page. Every check below is per menu, and table identities
    use source, heading and property names rather than volatile line numbers.
    """
    problems: list[str] = []
    for path, previous_menu in sorted(previous["menus"].items()):
        if path not in current["menus"]:
            problems.append(f"removed menu: {path}")
            continue
        current_menu = current["menus"][path]
        missing = sorted(set(previous_menu["properties"]) - set(current_menu["properties"]))
        if missing:
            problems.append(f"removed properties for {path}: {', '.join(missing)}")

        was_url, now_url = previous_menu["source_url"], current_menu["source_url"]
        if was_url and not now_url:
            problems.append(f"menu lost its source URL: {path} (was {was_url})")
        elif was_url and now_url and was_url != now_url:
            problems.append(f"menu source URL changed: {path} ({was_url} -> {now_url})")

        previous_rank = _COVERAGE_RANK.get(previous_menu["coverage"], -1)
        current_rank = _COVERAGE_RANK.get(current_menu["coverage"], -1)
        if current_rank < previous_rank:
            problems.append(
                f"menu coverage downgraded: {path} "
                f"({previous_menu['coverage']} -> {current_menu['coverage']})"
            )

        was_unattributed = previous_menu["unattributed_properties"]
        now_unattributed = current_menu["unattributed_properties"]
        if now_unattributed > was_unattributed:
            problems.append(
                f"properties lost their source for {path} ({was_unattributed} -> {now_unattributed})"
            )

    def unresolved(report):
        return {
            (table["source_url"], table["heading"], tuple(table["properties"]))
            for table in report["tables"] if table["status"] == "unresolved"
        }

    for url, heading, names in sorted(unresolved(current) - unresolved(previous)):
        problems.append(f"new unresolved table: {url or heading}: {', '.join(names)}")
    return problems


def load_report(path: Path) -> dict:
    """Reject malformed baselines with an actionable CLI error, not a traceback."""
    report = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(report, dict):
        raise ValueError("report must be a JSON object")
    if report.get("schema_version") != REPORT_SCHEMA_VERSION:
        raise ValueError(
            f"unsupported report schema {report.get('schema_version')!r}; regenerate the baseline "
            f"with this extractor (schema {REPORT_SCHEMA_VERSION})"
        )
    menus, tables = report.get("menus"), report.get("tables")
    if not isinstance(menus, dict) or not isinstance(tables, list):
        raise ValueError("report must contain menus and tables")
    for menu_path, menu in menus.items():
        if not isinstance(menu_path, str) or not isinstance(menu, dict):
            raise ValueError("menu entries must be objects keyed by path")
        names, coverage = menu.get("properties"), menu.get("coverage")
        if not isinstance(names, list) or any(not isinstance(name, str) for name in names):
            raise ValueError("menu properties must be lists of strings")
        if coverage not in COVERAGE_VALUES:
            raise ValueError(f"unknown coverage marker for {menu_path}: {coverage!r}")
        if not isinstance(menu.get("source_url"), str):
            raise ValueError("menu source_url must be a string")
        if not isinstance(menu.get("unattributed_properties"), int):
            raise ValueError("menu unattributed_properties must be an integer")
    for table in tables:
        if (not isinstance(table, dict) or table.get("status") not in ("resolved", "unresolved")
                or not isinstance(table.get("source_url"), str) or not isinstance(table.get("heading"), str)
                or not isinstance(table.get("properties"), list)
                or any(not isinstance(name, str) for name in table["properties"])):
            raise ValueError("malformed table evidence")
    return report
