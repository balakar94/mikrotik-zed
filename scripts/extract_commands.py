#!/usr/bin/env python3
"""
Extract RouterOS CLI command data from llms-full.txt and generate commands.toml.

Parses the CLI Reference section of llms-full.txt to extract menu paths,
argument names, types, descriptions, and flags for the COMPLETE RouterOS
CLI surface (~1000+ menus). All roots are discovered via the CLI-path regex
(^[a-z0-9][a-z0-9/_-]*$) rather than a hardcoded whitelist. Child pages are
titled WITH a slash (`## user/group`), but ROOT pages appear WITHOUT one
(`## user`, `## log`); such bare candidates are kept only when a `**Type:**`
line confirms them before the next heading of any level — prose subheadings
(`balance-xor`, ...) never carry one.

Output: data/commands.toml (TOML format for the Zed language server)
"""

import hashlib
import html
import os
import re
import sys
import tempfile
import tomllib
from datetime import datetime, timezone
from pathlib import Path


# CLI path regex: must look like a RouterOS menu path, e.g. "ip/firewall/filter"
# or "caps-man/interface". Must be lowercase alphanumeric with / _ - separators,
# at least one segment, no spaces. The leading "/" is added separately.
_CLI_PATH_RE = re.compile(r"^[a-z0-9][a-z0-9/_-]*$")

# Enum member extraction: captures the body between the parentheses of an
# `enum (a | b | c)` type declaration. Anything after the closing paren
# (e.g. a bit-map suffix `{ name:0, other:1 }`) is intentionally ignored,
# because the capture stops at the first `)`.
_ENUM_VALUES_RE = re.compile(r"enum \(([^)]*)\)")

# Header policy: how many distinct root segments to list in the `# Covers:`
# line before collapsing the rest into an ellipsis.
_MAX_COVERED_ROOTS = 12

# Explicit deny list — empty for complete coverage. Keep as set for future use.
_DENY_ROOTS: set[str] = set()

# ── Markdown property-table parsing ───────────────────────────────────────
# Upstream ships the same menu surface in two shapes:
#   1. CLI-reference pages: HTML-like <ArgTable>/<ArgTableRow> blocks.
#   2. Topic pages: GitHub pipe tables
#      (`| **name** (*type*; Default: ...) | description |`), tied to a
#      menu by a `**Sub-menu:** \`/path\`` line (or a CLI-path heading).
# The extractor historically read only (1), a latent blind spot: documented
# rows such as `!comments` were absent and richer DHCPv6 descriptions were
# dropped. The constants/helpers below parse (2) conservatively and merge it
# additively into the ArgTable-derived menus.
#
# Only property sections are merged. Command tables ("Menu specific
# commands"), print filters, and unrelated sections (e.g. certificate
# export/import parameter tables) are ignored, per the "do not invent
# entries" rule; the universal `print` command is captured separately.
_MD_PROPERTY_HEADER = "property"          # first header cell of a property table
_MD_PARAMETER_HEADER = "parameter"        # first header cell of a parameter table
_MD_PROPERTY_HEADING = "propert"          # heading marker: property section
_MD_COMMAND_HEADING = "command"           # heading marker: command section (skip)
_MD_PRINT_HEADING = "print parameter"     # heading of the common print-command table
_MD_TABLE_SEP_RE = re.compile(r"^\|[\s:|-]+\|?\s*$")
_MD_SUB_MENU_RE = re.compile(r"^\*\*Sub-menu:\*\*\s*(.+)$")
# First cell `**name** (rest)`, tolerating 2-4 bold markers and no space.
_MD_NAME_RE = re.compile(r"^\*{2,4}\s*([^*]+?)\s*\*{2,4}\s*(.*)$", re.DOTALL)
# Property names are single RouterOS-style tokens: lowercase alphanumerics
# plus `! _ . -` (e.g. `802.3-sap`, `!comments`, `use-peer-dns`). Anything
# else (grouped alternatives, sub-table labels, TitleCase status columns) is
# not a property name and is skipped.
_MD_VALID_NAME_RE = re.compile(r"^[a-z0-9!][a-z0-9!._-]*$")
# Standard RouterOS verbs that a few docs pages list inside "Properties"
# tables (e.g. /system/resource/irq/rps lists disable/edit/enable/reset).
# They are commands, not properties. Mirrors MenuData::STANDARD_VERBS in
# lsp/src/menus.rs, minus `comment`, which is a genuine property name.
_MD_COMMAND_NAMES: frozenset[str] = frozenset({
    "add", "remove", "set", "get", "print", "enable", "disable", "find",
    "move", "export", "import", "edit", "reset", "force-update",
})


def should_include(menu_path: str) -> bool:
    """Check if a menu path should be included — COMPLETE coverage.

    Includes every heading that looks like a CLI path:
      - starts with "/"
      - no spaces
      - inner path (without leading "/") matches ^[a-z0-9][a-z0-9/_-]*$
      - not in DENY list

    This covers all 30+ roots discovered in llms-full.txt (970 menus: ~510 Directory + ~460 Command).
    Historical whitelist behavior is preserved only for explicit DENY.
    """
    if not menu_path:
        return False
    stripped = menu_path.strip()
    if not stripped.startswith("/"):
        return False
    if " " in stripped:
        return False
    # Reject paths with uppercase, dots, or other non-CLI chars beyond allowlist
    # (e.g. "/Backup/Restore" or "Container - ThingsBoard MQTT/HTTP server" are NOT CLI)
    inner = stripped.lstrip("/")
    if not inner:
        return False
    if inner in _DENY_ROOTS or f"/{inner.split('/')[0]}" in _DENY_ROOTS:
        return False
    # Must match lowercase CLI regex; case-sensitive: uppercase fails
    if not _CLI_PATH_RE.match(inner):
        return False
    # Single-segment roots like "/certificate" are valid; multi-segment requires at least one "/"
    # but we allow both. If caller wants at least one "/", require "/" in inner for multi?
    # Keep allow for both to satisfy /certificate inclusion tests.
    return True


HEADING_RE = re.compile(r"^#{2,4}\s+(.+)")


def _normalize_heading_text(line: str) -> str | None:
    """Return normalized heading text for a ##..#### line, or None if not a heading.

    Normalization: strip surrounding whitespace, remove markdown links, and
    strip trailing dots. Shared by the slash-path and bare-root extractors so
    both operate on identical text.
    """
    m = HEADING_RE.match(line)
    if not m:
        return None
    text = m.group(1).strip()
    text = re.sub(r"\[.*?\]\(.*?\)", "", text).strip()
    return text.rstrip(".")


def _extract_heading_path(line: str) -> str | None:
    """Extract a RouterOS menu path from a markdown heading, or None.

    Handles ##, ###, #### headings; strips markdown links and trailing dots;
    returns None for non-menu headings (no '/' or starts with '#').
    """
    path = _normalize_heading_text(line)
    if path is not None and "/" in path and not path.startswith("#"):
        return path
    return None


def _extract_bare_cli_root(line: str) -> str | None:
    """Extract a slash-less CLI root word from a markdown heading, or None.

    Upstream titles ROOT pages without a leading slash (`## user`, `## log`,
    `## certificate`). A heading qualifies as a bare-root candidate only if,
    after the same normalization as _extract_heading_path(), its text is a
    single lowercase CLI word matching _CLI_PATH_RE with no '/' inside.
    Slash-bearing headings belong to _extract_heading_path(); uppercase prose
    (`Overview`) and multi-word prose (`print parameters`) fail the regex.

    Being a candidate does NOT make a heading a menu — parse_llms_full()
    additionally requires a `**Type:**` line before the next heading of any
    level, which real root pages always carry and prose subheadings never do.
    """
    text = _normalize_heading_text(line)
    if not text or text.startswith("#"):
        return None
    if "/" in text:
        return None
    if not _CLI_PATH_RE.match(text):
        return None
    return text


def _is_includable_menu(menu: dict | None) -> bool:
    """Decide whether a finished menu entry is appended to the result.

    Two gates:
      1. the path must pass should_include(), and
      2. bare-root candidates must have been confirmed by a `**Type:**` line
         (their "needs_type" flag cleared). Slash-derived menus never carry
         the flag, so gate 2 is a no-op for them.
    Used at both flush points (next accepted heading and end of file).
    """
    if not menu:
        return False
    if menu.get("needs_type"):
        return False
    return should_include(menu["path"])


def extract_enum_values(typ: str) -> list[str]:
    """Extract enum members from a RAW type string, before any truncation.

    Examples:
      "enum (none)"                                        -> ["none"]
      "enum (mac:ssid | mac | ssid)"                       -> ["mac:ssid", "mac", "ssid"]
      "enum (a | b) { a:0, b:1 }"                          -> ["a", "b"]   (bit-map suffix ignored)
      "enum (as-username | as-username-and-password)"      -> ["as-username", "as-username-and-password"]

    Non-enum types yield an empty list. Members are stripped of surrounding
    whitespace and empty members are dropped (e.g. from trailing separators).
    """
    m = _ENUM_VALUES_RE.search(typ)
    if not m:
        return []
    return [part.strip() for part in m.group(1).split("|") if part.strip()]


def _split_markdown_cells(line: str) -> list[str]:
    """Split one pipe-table line into cells, honouring `\\|` escapes.

    Markdown escapes literal pipes inside cells with a backslash (the type
    column uses them heavily, e.g. `*yes \\| no*`). Surrounding pipes are
    dropped; the escape is restored as a literal `|` after splitting.
    """
    text = line.strip()
    if text.startswith("|"):
        text = text[1:]
    if text.endswith("|"):
        text = text[:-1]
    text = text.replace("\\|", "\x00")
    return [cell.strip().replace("\x00", "|") for cell in text.split("|")]


def _clean_markdown_text(text: str) -> str:
    """Flatten a table cell to plain text for the TOML description field.

    Unescapes HTML entities, unwraps markdown links to their label, drops
    emphasis/backtick markers, and collapses whitespace. Leading/trailing
    spaces are removed so empty cells stay empty.
    """
    text = html.unescape(text)
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)
    text = text.replace("**", "").replace("*", "").replace("`", "")
    return re.sub(r"\s+", " ", text).strip()


def _extract_sub_menu_paths(line: str) -> list[str]:
    """Return every CLI path listed on a `**Sub-menu:**` line.

    Handles one-or-more backticked paths separated by commas (some pages
    document a shared property set for several menus, e.g.
    `/interface/bridge/filter, /interface/bridge/nat`). Unknown shapes and
    empty tokens are dropped.
    """
    match = _MD_SUB_MENU_RE.match(line.strip())
    if not match:
        return []
    paths: list[str] = []
    for token in re.findall(r"`([^`]+)`", match.group(1)):
        for part in token.split(","):
            part = part.strip()
            if not part:
                continue
            if not part.startswith("/"):
                part = "/" + part
            inner = part.lstrip("/")
            if inner and " " not in part and _CLI_PATH_RE.match(inner):
                paths.append(part)
    return paths


def _parse_markdown_type(remainder: str) -> tuple[str, bool]:
    """Parse `(type; Default: ...)` from a cell remainder.

    Returns `(type, read_only)`. The default clause is stripped, markdown
    emphasis removed, and the `read-only` marker (when present) both reported
    and removed from the type. Malformed captures that still contain
    parentheses are discarded rather than shipped.
    """
    match = re.search(r"\(([^()]*)\)", remainder)
    if not match:
        return "", False
    inner = match.group(1)
    inner = re.split(r"[;,]?\s*[Dd]efault\s*:", inner, maxsplit=1)[0]
    inner = inner.replace("*", "").replace("`", "").strip()
    read_only = inner.lower().startswith("read-only")
    if read_only:
        inner = inner[len("read-only"):].lstrip(";: ").strip()
    if "(" in inner or ")" in inner:
        inner = ""
    return inner, read_only


def _parse_markdown_property_row(
    cells: list[str], desc_idx: int, type_idx: int | None
) -> dict | None:
    """Parse one pipe-table row into a property entry, or None.

    Returns `{name, type, read_only, description}`. The name must match
    `_MD_VALID_NAME_RE`; rows that are grouped alternatives, sub-table labels
    or prose are rejected (None), never invented as properties.
    """
    if not cells:
        return None
    name_match = _MD_NAME_RE.match(cells[0].strip())
    if not name_match:
        return None
    name = name_match.group(1).strip()
    if not _MD_VALID_NAME_RE.match(name):
        return None
    remainder = name_match.group(2)
    read_only = False
    typ = ""
    if type_idx is not None and 0 <= type_idx < len(cells):
        typ = _clean_markdown_text(cells[type_idx])
        if "(" in typ or ")" in typ:
            typ = ""
    if not typ:
        typ, read_only = _parse_markdown_type(remainder)
    else:
        read_only = _parse_markdown_type(remainder)[1]
    description = _clean_markdown_text(cells[desc_idx]) if 0 <= desc_idx < len(cells) else ""
    return {
        "name": name,
        "type": typ,
        "read_only": read_only,
        "description": description,
    }


def _collect_markdown_table(
    header_cells: list[str],
    rows: list[str],
    paths: list[str],
    heading: str,
    line_no: int,
    md_tables: list[dict],
    print_rows: list[dict],
    warnings: list[str],
) -> None:
    """Classify one pipe table and stash its rows for later merging.

    Property sections (heading contains `propert`) contribute to `md_tables`
    keyed by their associated menu paths. Command sections are skipped. A
    `print parameters` parameter table contributes to `print_rows` (captured
    later as the universal `/print` command). Unrelated parameter tables and
    sections without a `**Sub-menu:**` association are ignored.
    """
    low = heading.lower()
    first = header_cells[0].strip().lower()
    header = [cell.strip().lower() for cell in header_cells]
    type_idx = header.index("type") if "type" in header else None
    if "description" in header:
        desc_idx = header.index("description")
    else:
        desc_idx = 1 if len(header_cells) > 1 else 0

    if first == _MD_PARAMETER_HEADER:
        if _MD_PRINT_HEADING not in low:
            return
        for raw in rows:
            parsed = _parse_markdown_property_row(_split_markdown_cells(raw), desc_idx, type_idx)
            if parsed is not None:
                print_rows.append({
                    "name": parsed["name"],
                    "type": parsed["type"],
                    "description": parsed["description"],
                })
        return

    if _MD_COMMAND_HEADING in low or _MD_PROPERTY_HEADING not in low:
        return

    section = "read_only" if (
        "read-only" in low or "read only" in low or "readonly" in low
    ) else "arguments"
    parsed_rows: list[dict] = []
    for raw in rows:
        cells = _split_markdown_cells(raw)
        parsed = _parse_markdown_property_row(cells, desc_idx, type_idx)
        if parsed is None:
            # Only rows whose bold marker is malformed count as an unknown
            # row kind; grouped alternatives (`**a | b**`), TitleCase status
            # columns and sub-table labels are deliberately not properties.
            malformed = (
                cells
                and cells[0].strip().startswith("*")
                and _MD_NAME_RE.match(cells[0].strip()) is None
            )
            if malformed:
                warnings.append(
                    f"unknown markdown property row near line {line_no}: {raw.strip()[:60]!r}"
                )
            continue
        row_section = "read_only" if (parsed["read_only"] or section == "read_only") else "arguments"
        parsed["section"] = row_section
        parsed_rows.append(parsed)

    if not paths:
        return
    if not parsed_rows:
        warnings.append(
            f"property table near line {line_no} for {', '.join(paths)} yielded no rows "
            f"(heading {heading!r}); upstream shape may have drifted"
        )
        return
    md_tables.append({"paths": list(paths), "rows": parsed_rows})


def _merge_markdown_rows(menu: dict, rows: list[dict], known_paths: set[str]) -> int:
    """Merge markdown rows into one menu; return the number of rows added.

    Never duplicates an existing name in any section; for an existing entry
    the richer (non-empty/longer) description and a type fill an empty one.
    Sub-menu links (`port`, `status`) and documented command verbs are not
    properties and are skipped.
    """
    existing: dict[str, tuple[str, dict]] = {}
    for section in ("arguments", "flags", "read_only"):
        for entry in menu.get(section, []) or []:
            existing.setdefault(entry.get("name", ""), (section, entry))

    added = 0
    for row in rows:
        name = row["name"]
        target = row.get("section", "arguments")
        if name in existing:
            entry = existing[name][1]
            description = row.get("description", "")
            if description and (
                not entry.get("description") or len(description) > len(entry["description"])
            ):
                entry["description"] = description
            if row.get("type") and not entry.get("type"):
                entry["type"] = row["type"]
            continue
        # Generic add-item properties are owned by GENERIC_ITEM_PROPS and
        # injected later; adding a markdown row here (often with an empty
        # description) would shadow the generic text.
        if name in GENERIC_NAMES:
            continue
        # A child menu with this name is a sub-menu link, not a property.
        if f"{menu['path']}/{name}" in known_paths:
            continue
        if name in _MD_COMMAND_NAMES:
            continue
        entry = {
            "name": name,
            "type": row.get("type", ""),
            "required": False,
            "unset": False,
            "description": row.get("description", ""),
        }
        menu.setdefault(target, []).append(entry)
        existing[name] = (target, entry)
        added += 1
    return added


def _append_argtable_row(
    text: str, current_menu: dict | None, current_section: str | None
) -> None:
    """Parse one `<ArgTableRow>` block (possibly multi-line) and append it.

    `text` may span several physical lines so attributes whose quoted value
    contains a newline (e.g. `typ="multi { array-id, option: enum\\n }"`) are
    captured intact instead of being dropped. Anonymous rows are skipped.
    """
    if current_menu is None:
        return
    arg_match = re.search(r'arg="([^"]+)"', text)
    if arg_match is None or not arg_match.group(1):
        return
    typ_match = re.search(r'typ="([^"]*)"', text)
    mandatory_match = re.search(r'mandatory="1"', text)
    unset_match = re.search(r'unset="1"', text)
    desc_match = re.search(r">([^<]*)</ArgTableRow", text)
    description = html.unescape(desc_match.group(1).strip()) if desc_match else ""
    entry = {
        "name": arg_match.group(1),
        "type": typ_match.group(1) if typ_match else "",
        "required": bool(mandatory_match),
        "unset": bool(unset_match),
        "description": description,
    }
    enum_values = extract_enum_values(entry["type"])
    if enum_values:
        entry["enum_values"] = enum_values
    if current_section == "flags":
        current_menu["flags"].append(entry)
    elif current_section == "arguments":
        current_menu["arguments"].append(entry)
    elif current_section == "readonly":
        current_menu["read_only"].append(entry)


def _build_print_command(print_rows: list[dict]) -> dict:
    """Build the synthetic `/print` Command from the common print table.

    `print` is a verb, not a menu, so its documented parameters (including
    the `!comments` filter) have no ArgTable home. Recording them once on a
    `/print` Command keeps completion/diagnostics aware of them without
    injecting 17 print rows into every menu's hover card. The parameters are
    bare tokens, so they are stored as flags (print-output modifiers).
    """
    menu = {"path": "/print", "type": "Command", "flags": [], "arguments": [], "read_only": []}
    for row in print_rows:
        menu["flags"].append({
            "name": row["name"],
            "type": row.get("type", ""),
            "required": False,
            "unset": False,
            "description": row.get("description", ""),
        })
    return menu


def parse_llms_full(filepath: str) -> list[dict]:
    """Parse llms-full.txt and extract menu entries.

    Reads both table shapes upstream ships:
      - `<ArgTable>`/`<ArgTableRow>` CLI-reference blocks (authoritative for
        menu paths, flags, arguments and read-only rows);
      - GitHub pipe tables on topic pages, tied to a menu by a
        `**Sub-menu:** \\`/path\\`` line or a CLI-path heading.

    Markdown rows are merged additively after the scan: a non-empty ArgTable
    type is never overwritten, an empty one is filled, and the richer
    description wins (see `_merge_markdown_rows`). Multi-line
    `<ArgTableRow ...>` openings (attributes whose quoted value spans a
    newline) are buffered until the tag closes.
    """
    with open(filepath, "r", encoding="utf-8") as f:
        content = f.read()

    menus: list[dict] = []
    current_menu = None
    current_section = None  # "flags", "arguments", or "readonly"
    in_argtable = False
    warnings: list[str] = []

    # Markdown association state: the page heading drives which section a
    # pipe table belongs to, while `**Sub-menu:**` / path headings drive the
    # target menu(s).
    effective_paths: list[str] = []
    current_heading = ""
    md_tables: list[dict] = []
    print_rows: list[dict] = []
    md_skip_until = -1
    pending_row: list[str] | None = None

    lines = content.split("\n")

    for i, line in enumerate(lines):
        # Finish a multi-line <ArgTableRow> before anything else: the buffered
        # block may otherwise contain heading-looking attribute text.
        if pending_row is not None:
            pending_row.append(line)
            if "</ArgTableRow>" in line:
                _append_argtable_row("\n".join(pending_row), current_menu, current_section)
                pending_row = None
            continue

        # Pipe-table body rows are consumed by their header's collection pass.
        if i <= md_skip_until:
            continue

        # Track the current heading text (pipe-table classification) and, for
        # topic pages, the menu path association. A `##` heading always starts
        # a new page; `###`/`####` subsections keep the enclosing association.
        heading_text = _normalize_heading_text(line)
        heading_path = _extract_heading_path(line)
        bare_root = _extract_bare_cli_root(line) if heading_path is None else None
        if heading_text is not None:
            current_heading = heading_text
            if heading_path is not None:
                effective_paths = ["/" + heading_path.lstrip("/")]
            elif bare_root is not None:
                effective_paths = []
            elif line.startswith("## "):
                effective_paths = []

        if heading_path is not None or bare_root is not None:
            # Save previous menu if it exists and qualifies for inclusion
            if _is_includable_menu(current_menu):
                menus.append(current_menu)

            new_path = heading_path if heading_path is not None else bare_root
            current_menu = {
                "path": "/" + new_path.lstrip("/"),  # Add leading / (once)
                "type": "Directory",
                "flags": [],
                "arguments": [],
                "read_only": [],
            }
            if bare_root is not None:
                # Bare-root candidate: unconfirmed until a **Type:** line
                # appears within this section. Slash paths never set the flag.
                current_menu["needs_type"] = True
            current_section = None
            in_argtable = False
            continue

        # `**Sub-menu:**` switches the markdown association for its page.
        sub_paths = _extract_sub_menu_paths(line)
        if sub_paths:
            effective_paths = sub_paths

        # Detect Type
        type_match = re.match(r"^\*\*Type:\*\*\s+(.+)", line)
        if type_match and current_menu:
            current_menu["type"] = type_match.group(1).strip()
            # Confirms a pending bare-root candidate; pop() keeps confirmed
            # and slash-derived menus free of the flag key entirely.
            current_menu.pop("needs_type", None)
            continue

        # Detect ArgTable end first (before start, since </ArgTable> also contains <ArgTable)
        if "</ArgTable>" in line:
            in_argtable = False
            current_section = None
            continue

        # Detect ArgTableRow (before ArgTable, since <ArgTableRow contains <ArgTable)
        if in_argtable and current_menu and "<ArgTableRow" in line:
            if "</ArgTableRow>" in line:
                _append_argtable_row(line, current_menu, current_section)
            else:
                # Attributes span a newline: buffer until the tag closes.
                pending_row = [line]
            continue

        # Detect ArgTable start
        if "<ArgTable" in line:
            in_argtable = True
            c1_match = re.search(r'c1="([^"]+)"', line)
            c1 = c1_match.group(1) if c1_match else None
            if c1 == "Flag":
                current_section = "flags"
            elif c1 == "Argument":
                current_section = "arguments"
            elif c1 is not None and "Read-only" in c1:
                current_section = "readonly"
            else:
                # Unknown column kind: ignore its rows (never guess a section)
                # but make the drift visible instead of silent.
                current_section = None
                if c1 is not None:
                    where = current_menu["path"] if current_menu else "<no menu>"
                    warnings.append(
                        f"unknown ArgTable c1={c1!r} near line {i + 1} ({where}); rows ignored"
                    )
            continue

        # Markdown pipe table: property and parameter headers start a table.
        if line.lstrip().startswith("|") and not _MD_TABLE_SEP_RE.match(line):
            cells = _split_markdown_cells(line)
            if cells and cells[0].strip().lower() in (_MD_PROPERTY_HEADER, _MD_PARAMETER_HEADER):
                end = i + 1
                rows: list[str] = []
                while end < len(lines) and lines[end].startswith("|"):
                    if not _MD_TABLE_SEP_RE.match(lines[end]):
                        rows.append(lines[end])
                    end += 1
                md_skip_until = end - 1
                _collect_markdown_table(
                    cells, rows, effective_paths, current_heading, i + 1,
                    md_tables, print_rows, warnings,
                )
            continue

    # EOF: flush a dangling multi-line row and the last menu.
    if pending_row is not None:
        _append_argtable_row("\n".join(pending_row), current_menu, current_section)
    if _is_includable_menu(current_menu):
        menus.append(current_menu)

    # Additive merge of markdown rows into the ArgTable-derived menus.
    if md_tables:
        known_paths = {m["path"] for m in menus}
        by_path: dict[str, list[dict]] = {}
        for menu in menus:
            by_path.setdefault(menu["path"], []).append(menu)
        merged = 0
        for table in md_tables:
            for path in table["paths"]:
                for menu in by_path.get(path, []):
                    merged += _merge_markdown_rows(menu, table["rows"], known_paths)
        if merged:
            print(
                f"info: merged {merged} markdown property row(s) from topic pages.",
                file=sys.stderr,
            )

    # `print` is a verb, not a menu: record its documented common parameters
    # (including `!comments`) once as a synthetic Command entry.
    if print_rows:
        menus.append(_build_print_command(print_rows))

    for warning in warnings:
        print(f"warning: {warning}", file=sys.stderr)

    return menus


def clean_type(typ: str) -> str:
    """Clean and simplify type strings for the TOML output."""
    # Remove excessive whitespace
    typ = re.sub(r"\s+", " ", typ).strip()
    # Truncate very long type descriptions — 150 keeps complex alt/super/multi types
    # (often 110-140 chars) intact while still capping extremes; enum_values already
    # preserves full member lists so display truncation can be generous.
    if len(typ) > 150:
        # Preserve closing `)` for `enum (...)` types so truncated values remain
        # syntactically balanced. If original starts with `enum (` and is longer
        # than cap, end with `...)` rather than bare `...`.
        if typ.lstrip().startswith("enum ("):
            typ = typ[:146] + "...)"
        else:
            typ = typ[:147] + "..."
    return typ


def escape_toml_string(s: str) -> str:
    """Escape a string for TOML basic string representation.

    Handles TOML-required escapes: backslash, quote, tab, and control
    characters 0x00-0x1F (as \\u00XX) except \\n/\\r which are stripped
    and \\t which is escaped as \\t. Keeps deleting \\n/\\r for
    single-line TOML values but ensures \\t doesn't produce invalid TOML.
    """
    out: list[str] = []
    for ch in s:
        if ch == "\\":
            out.append("\\\\")
        elif ch == '"':
            out.append('\\"')
        elif ch == "\t":
            out.append("\\t")
        elif ch == "\n":
            out.append(" ")
        elif ch == "\r":
            continue
        elif 0x00 <= ord(ch) <= 0x1F:
            out.append(f"\\u{ord(ch):04X}")
        else:
            out.append(ch)
    return "".join(out)


def _extract_routeros_version(llms_path: Path) -> str:
    """Extract RouterOS version string, preferring the provenance manifest."""
    # Prefer the synced manifest: it records the highest feature-gate mention
    # across the whole corpus, not just the header, and stays in sync with
    # data/upstream-docs.toml (see sync_llms.py). Fall back to scanning the
    # llms-full.txt header only when the manifest is absent (e.g. fresh clone
    # before first `make sync`).
    try:
        manifest = (llms_path.parent / "data" / "upstream-docs.toml")
        if manifest.exists():
            m = re.search(r'routeros_version\s*=\s*"([^"]+)"', manifest.read_text(encoding="utf-8"))
            if m and m.group(1) not in ("", "unknown"):
                return m.group(1)
    except Exception:
        pass
    try:
        text = llms_path.read_text(encoding="utf-8", errors="ignore")[:8192]
        # Try common patterns: "RouterOS 7.22", "RouterOS v7.22", "7.22" in first lines
        m = re.search(r"RouterOS\s+v?(\d+\.\d+(?:\.\d+)?)", text, re.IGNORECASE)
        if m:
            return m.group(1)
        m = re.search(r"\b7\.\d+(?:\.\d+)?\b", text)
        if m:
            return m.group(0)
    except Exception:
        pass
    return "7.22+"


def _source_hash(llms_path: Path) -> str:
    """Compute sha256 hash of llms-full.txt for reproducibility."""
    try:
        h = hashlib.sha256()
        with open(llms_path, "rb") as f:
            for chunk in iter(lambda: f.read(8192), b""):
                h.update(chunk)
        return h.hexdigest()[:16]
    except Exception:
        return "unknown"


def _covers_line(menus: list[dict]) -> str:
    """Build the `# Covers:` header line from the included menus.

    Lists the sorted, deduplicated root segments (`/ip`, `/interface`, ...)
    capped at `_MAX_COVERED_ROOTS` entries followed by an ellipsis, and ends
    with the real menu count so the header never drifts from the payload.
    """
    roots = sorted({m["path"].split("/")[1] for m in menus if len(m["path"].split("/")) > 1})
    shown = [f"/{r}" for r in roots][:_MAX_COVERED_ROOTS]
    body = ", ".join(shown)
    if len(roots) > _MAX_COVERED_ROOTS:
        body += ", …"
    if body:
        return f"# Covers: {body} ({len(menus)} menus)"
    return f"# Covers: ({len(menus)} menus)"


def synthesize_directories(menus: list[dict]) -> list[dict]:
    """Return `menus` plus a bare Directory entry for every missing ancestor.

    WHY: upstream RouterOS docs no longer publish standalone Directory
    sections, so intermediate menus (/ip, /routing/ospf, ...) vanish from
    llms-full.txt whenever only their children have their own pages.
    Prefix completion and unknown-menu diagnostics still need every level
    of the hierarchy, so each proper ancestor prefix absent from the
    explicit set is added as `{path, type: "Directory"}` — no flags, no
    arguments, no read-only rows, hence no description in the output.
    (The empty lists keep the entry shape-compatible with generate_toml;
    they emit nothing.)

    Gaps only: explicitly parsed entries are never overwritten or merged
    into. Multi-level chains are handled transitively (/a/b/c implies
    /a/b and /a). Input order does not matter; callers sort afterwards.

    Note on "/root": upstream llms-full.txt contains a single leaf
    "## root/terminal" with **Type: Directory** (a docs artifact — there
    is no real RouterOS CLI menu "/root"). The synthesizer intentionally
    keeps "/root" as an empty Directory whose only child is "terminal" to
    preserve hierarchy integrity: without it, "/root/terminal" would have
    a dangling ancestor and prefix completion / ancestor_prefixes would
    treat "/root" as unknown even though its child is documented. The
    entry is harmless (no flags/arguments/read-only) and diagnostics
    correctly treat "/root" as known; filtering it out would break docs
    fidelity for no benefit, so it is kept intentionally.
    """
    known_paths = {m["path"] for m in menus}
    synthesized: dict[str, dict] = {}
    for menu in menus:
        segments = menu["path"].split("/")[1:]  # drop "" before the leading "/"
        # Proper ancestors only: /a/b/c contributes /a and /a/b, never itself.
        for i in range(1, len(segments)):
            ancestor_path = "/" + "/".join(segments[:i])
            if ancestor_path not in known_paths:
                synthesized[ancestor_path] = {
                    "path": ancestor_path,
                    "type": "Directory",
                    "flags": [],
                    "arguments": [],
                    "read_only": [],
                }
    return menus + list(synthesized.values())


def _dedupe_entries(
    entries: list[dict],
    path: str,
    section: str,
    duplicates: list[tuple[str, str, str]] | None = None,
) -> list[dict]:
    """Deduplicate entries by name, first-wins — weight/speed optimization.

    Upstream llms-full.txt concatenates two generations of docs for
    /interface/wifi (ssid/mode/etc appear twice with different types).
    Keeping duplicates bloats TOML and completion lists. First-wins preserves
    backwards compat with existing snapshots; duplicates are tracked and summarized.
    Single source of truth: applied only at the TOML generation boundary.
    """
    seen: set[str] = set()
    unique: list[dict] = []
    for e in entries:
        name = e.get("name", "")
        if name in seen:
            if duplicates is not None:
                duplicates.append((path, section, name))
            continue
        seen.add(name)
        unique.append(e)
    return unique


def finalize_menus(menus: list[dict]) -> list[dict]:
    """Normalize parsed menus for serialization: dedupe, fill gaps, sort.

    Applies, in order:
      1. Deduplicate by path (first occurrence wins).
      2. Synthesize Directory entries for ancestors missing upstream.
      3. Sort by path — what keeps repeated runs byte-identical.

    One function so main() and the tests share a single definition of the
    pre-serialization pipeline instead of duplicating its steps.
    """
    seen: set[str] = set()
    unique = []
    for m in menus:
        if m["path"] not in seen:
            seen.add(m["path"])
            unique.append(m)

    unique = synthesize_directories(unique)
    unique.sort(key=lambda m: m["path"])
    return unique


# Universal add-item properties (Common commands): upstream llms-full.txt
# documents `comment`, `disabled`, `place-before`, and `copy-from` only as
# prose in the "Common commands" add-row description, not in per-menu
# ArgTables (tabulated ~0-3 times), so per-menu extraction misses them and
# diagnostics surface them as unknown-property on real item menus
# (e.g. /ipv6/firewall/mangle, /ipv6/nd/prefix, /system/scheduler).
# Injected additively for every menu with type exactly "Directory" at the
# top of generate_toml() (NOT in finalize_menus, so curated overrides keep
# precedence: overrides run first in main(), generics only fill gaps).
# Scope split: `comment`/`disabled` stay universal to all Directory menus
# (every addable item accepts them); `place-before`/`copy-from` are
# restricted to ordered item-list menus. Scope choice: explicit denylist
# over an ordered-item allowlist — an allowlist would need ~470 entries
# covering every item list and would rot on every upstream sync, while the
# denylist needs only a bare-ancestor rule plus a handful of known
# read-only tables. Bare ancestors (synthesized empty Directories such as
# /ip, /system, /tool, /caps-man, /console — no flags/arguments/read_only
# upstream) are containers, never ordered lists. Known read-only status
# tables (registration-table, remote-cap, hardware radio views) expose
# read-only rows but no ordered add semantics.
# Menus with type "Command" or "Settings Directory" (singleton settings
# such as /system/clock) are skipped entirely. A generic whose name already
# exists in arguments/flags/read_only is skipped (never modifies existing
# rows); note the `disabled` bool property is distinct from the
# single-letter `X` disabled print flag, so only an exact `disabled` name
# collides.
GENERIC_ITEM_PROPS: tuple[dict[str, str], ...] = (
    {
        "name": "comment",
        "type": "string",
        "description": "Descriptive comment for the item (universal add/set property from Common commands; omitted from upstream per-menu tables).",
    },
    {
        "name": "disabled",
        "type": "bool",
        "description": "Whether the item is disabled (universal add property from Common commands; distinct from the X disabled print flag).",
    },
    {
        "name": "place-before",
        "type": "string",
        "description": "Place a new item before the given item number (universal add property from Common commands for ordered item lists).",
    },
    {
        "name": "copy-from",
        "type": "string",
        "description": "Copy property values from an existing item (universal add property from Common commands).",
    },
)


# Split scope within GENERIC_ITEM_PROPS: universal vs. ordered-only names.
_GENERIC_UNIVERSAL_NAMES: tuple[str, ...] = ("comment", "disabled")
_GENERIC_ORDERED_NAMES: tuple[str, ...] = ("place-before", "copy-from")

GENERIC_NAMES: frozenset[str] = frozenset(g["name"] for g in GENERIC_ITEM_PROPS)

# Denylist for ordered-only generics (see scope-split comment above).
# Exact paths: hardware radio views with zero writable arguments (status
# tables, not ordered lists). Suffixes match any depth (e.g.
# /interface/wifi/registration-table and /interface/wireless/registration-table
# alike). NOTE: "/radio" is deliberately NOT a suffix — /interface/wifi/network/radio
# is a real configurable item list (20 upstream args) and keeps ordering props;
# only the two zero-arg radio status views are listed exactly. /caps-man and
# /console are bare ancestors covered by the empty-menu rule below and listed
# here explicitly only to pin the audited examples.
_NO_ORDER_EXACT: frozenset[str] = frozenset({
    "/caps-man",
    "/console",
    "/caps-man/radio",
    "/interface/wifi/radio",
})
_NO_ORDER_SUFFIXES: tuple[str, ...] = (
    "/registration-table",
    "/remote-cap",
)


def _is_bare_ancestor(menu: dict) -> bool:
    """True when a Directory carries no upstream rows at all.

    Synthesized ancestors (synthesize_directories) are born empty, and a few
    upstream pages (e.g. /caps-man, /console) are documented without any
    ArgTable. Either way the menu is a hierarchy container, never an ordered
    item list, so ordered-only generics do not apply.
    """
    return not menu.get("flags") and not menu.get("arguments") and not menu.get("read_only")


def _allows_ordered_generics(menu: dict) -> bool:
    """True when `place-before`/`copy-from` may be injected into `menu`.

    Denied for bare ancestors (empty rule above) and for known read-only
    tables in _NO_ORDER_EXACT / _NO_ORDER_SUFFIXES. Everything else with
    type Directory is treated as a (potentially) ordered item list — a
    missing `comment=` breaks diagnostics, while two extra completions on a
    borderline container are harmless.
    """
    if _is_bare_ancestor(menu):
        return False
    path = menu.get("path", "")
    if path in _NO_ORDER_EXACT:
        return False
    return not any(path.endswith(suffix) for suffix in _NO_ORDER_SUFFIXES)


def override_overlaps_generics(override: dict) -> bool:
    """True when an override property would also be injected as a generic.

    Overlap alone does not make the entry redundant: /ip/route+comment
    overlaps but is kept for its richer on-device description (see
    data/overrides.toml). Use override_fully_subsumed_by_generics() to detect
    entries that add nothing beyond the generic text.
    """
    return override.get("property") in GENERIC_NAMES


def override_fully_subsumed_by_generics(override: dict) -> bool:
    """True when an override adds nothing beyond the generic row.

    Fully subsumed = name overlaps a generic AND the override carries no
    richer payload (empty description, or byte-identical to the generic
    description). Such entries should be retired: the generic already ships
    the property, so the override only adds header noise. A richer
    description (like /ip/route+comment) is NOT subsumed and is kept.
    """
    prop = override.get("property")
    if prop not in GENERIC_NAMES:
        return False
    generic = next(g for g in GENERIC_ITEM_PROPS if g["name"] == prop)
    desc = (override.get("description") or "").strip()
    return not desc or desc == generic["description"]


def _generic_additions_for_menu(menu: dict) -> list[dict]:
    """Return the generic item properties missing from `menu` (pure, no mutation).

    Only menus with type exactly "Directory" qualify. `comment`/`disabled`
    apply universally; `place-before`/`copy-from` additionally require
    _allows_ordered_generics(). A generic is missing when its name appears
    in NONE of arguments/flags/read_only. Returned rows are fresh argument
    dicts (required/unset False) ready to emit.
    """
    if menu.get("type") != "Directory":
        return []
    existing: set[str] = set()
    for section in ("arguments", "flags", "read_only"):
        for entry in menu.get(section, []) or []:
            existing.add(entry.get("name", ""))
    allow_ordered = _allows_ordered_generics(menu)
    additions: list[dict] = []
    for generic in GENERIC_ITEM_PROPS:
        if generic["name"] not in existing:
            if generic["name"] in _GENERIC_ORDERED_NAMES and not allow_ordered:
                continue
            additions.append({
                "name": generic["name"],
                "type": generic["type"],
                "required": False,
                "unset": False,
                "description": generic["description"],
            })
            existing.add(generic["name"])
    return additions


def apply_generic_item_props(menus: list[dict]) -> int:
    """Append missing generic item properties to Directory menus; return count.

    Additive-only mutating helper for tests and offline use: new rows go to
    the menu's `arguments` list, existing names (in any section) are never
    touched. The production pipeline does NOT call this (generate_toml()
    emits generics purely without mutating its input, so repeated runs stay
    byte-identical); calling both would make the generate_toml header count
    read 0 because nothing is missing anymore.
    """
    applied = 0
    for menu in menus:
        for addition in _generic_additions_for_menu(menu):
            menu.setdefault("arguments", []).append(addition)
            applied += 1
    return applied


# Curated additive overrides (data/overrides.toml) — properties the upstream
# docs omit but real devices expose (verifiable via /export). Applied AFTER
# upstream parsing, recorded in the commands.toml header provenance
# (`# overrides_applied = N`). Additive ONLY: an override never modifies an
# upstream-derived entry. Conflict policy is ignore-with-warning (stderr):
# an override whose property already exists on the menu, or whose path is
# unknown upstream, is skipped with a warning so `make extract` stays green
# when upstream eventually documents the property (the warning tells the
# maintainer to drop the now-redundant entry). A malformed overrides file
# fails loudly via OverrideError (non-zero exit from main()).
_OVERRIDES_FILENAME = "overrides.toml"


class OverrideError(ValueError):
    """Raised when data/overrides.toml exists but cannot be applied."""


def load_overrides(overrides_path: Path) -> list[dict]:
    """Load curated overrides from `overrides_path`, or [] when absent.

    A missing file is fine (upstream-only output). Malformed TOML, a
    non-list `overrides` key, or an entry missing/invalid `path`/`property`
    raises OverrideError. Optional per-entry keys: `type` and `description`
    (both default to "").
    """
    try:
        raw = overrides_path.read_bytes()
    except FileNotFoundError:
        return []
    except OSError as e:
        raise OverrideError(f"cannot read {overrides_path}: {e}") from e
    try:
        data = tomllib.loads(raw.decode("utf-8"))
    except Exception as e:
        raise OverrideError(f"malformed {overrides_path}: {e}") from e
    if not isinstance(data, dict):
        raise OverrideError(f"malformed {overrides_path}: top level must be a table")
    entries = data.get("overrides", [])
    if not isinstance(entries, list):
        raise OverrideError(f"malformed {overrides_path}: 'overrides' must be a list")
    overrides: list[dict] = []
    for i, entry in enumerate(entries):
        if not isinstance(entry, dict):
            raise OverrideError(f"malformed {overrides_path}: overrides[{i}] must be a table")
        path = entry.get("path")
        prop = entry.get("property")
        typ = entry.get("type", "")
        desc = entry.get("description", "")
        if not isinstance(path, str) or not path.startswith("/") or " " in path:
            raise OverrideError(
                f"malformed {overrides_path}: overrides[{i}] needs a CLI 'path' (e.g. \"/ip/route\")"
            )
        if not isinstance(prop, str) or not prop or " " in prop:
            raise OverrideError(
                f"malformed {overrides_path}: overrides[{i}] needs a non-empty 'property' name"
            )
        if not isinstance(typ, str) or not isinstance(desc, str):
            raise OverrideError(
                f"malformed {overrides_path}: overrides[{i}] 'type'/'description' must be strings"
            )
        overrides.append({"path": path, "property": prop, "type": typ, "description": desc})
    return overrides


def apply_overrides(menus: list[dict], overrides: list[dict]) -> int:
    """Append override properties to matching menus; return the count applied.

    Additive-only: new properties go to the menu's `arguments` list. Skipped
    with a stderr warning (never modified): unknown paths (no invented
    menus) and properties already present in `arguments`/`flags`/`read_only`.
    Applied overrides that overlap a generic name keep precedence (generics
    only fill still-missing names downstream); an overlap is noted on stderr
    as info, and a fully-subsumed overlap (no richer description than the
    generic) warns so CI tells the maintainer to retire the entry.
    """
    by_path = {m["path"]: m for m in menus}
    applied = 0
    for o in overrides:
        menu = by_path.get(o["path"])
        if menu is None:
            print(f"warning: override skipped, unknown menu {o['path']!r}", file=sys.stderr)
            continue
        existing = set()
        for section in ("arguments", "flags", "read_only"):
            for e in menu.get(section, []):
                existing.add(e.get("name", ""))
        if o["property"] in existing:
            print(
                f"warning: override skipped, {o['path']!r} already documents {o['property']!r} "
                "(upstream now covers it — remove the redundant entry)",
                file=sys.stderr,
            )
            continue
        menu.setdefault("arguments", []).append({
            "name": o["property"],
            "type": o["type"],
            "required": False,
            "unset": False,
            "description": o["description"],
        })
        applied += 1
        if override_fully_subsumed_by_generics(o):
            print(
                f"warning: override {o['path']!r} {o['property']!r} is fully subsumed by generics "
                "(no richer description — remove the redundant entry)",
                file=sys.stderr,
            )
        elif override_overlaps_generics(o):
            print(
                f"info: override {o['path']!r} {o['property']!r} overlaps a generic item property "
                "(kept for description quality)",
                file=sys.stderr,
            )
    return applied


def generate_toml(
    menus: list[dict],
    llms_path: Path | None = None,
    overrides_applied: int = 0,
    generics_applied: int | None = None,
) -> str:
    """Generate TOML output from parsed menus.

    Universal add-item generics (GENERIC_ITEM_PROPS) are emitted here,
    purely: per-menu missing rows are computed via _generic_additions_for_menu
    without mutating the input, so repeated calls stay byte-identical modulo
    the Generated timestamp. Pass generics_applied explicitly only when the
    caller already previewed the count (main() does); None auto-computes it.
    """
    # Pure preview of generic rows per menu path (paths are unique after
    # finalize_menus). Computed once so the header count and the emitted rows
    # always agree, and the input list is never mutated.
    generic_map: dict[str, list[dict]] = {}
    auto_generics = 0
    for menu in menus:
        additions = _generic_additions_for_menu(menu)
        if additions:
            generic_map[menu["path"]] = additions
            auto_generics += len(additions)
    if generics_applied is None:
        generics_applied = auto_generics
    lines = []
    lines.append("# MikroTik RouterOS CLI Command Table")
    lines.append("# Auto-generated from llms-full.txt")
    lines.append(_covers_line(menus))
    # Metadata header
    if llms_path is not None and llms_path.exists():
        version = _extract_routeros_version(llms_path)
        src_hash = _source_hash(llms_path)
    else:
        version = "unknown"
        src_hash = "unknown"
    generated = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
    lines.append(f"# RouterOS version: {version}")
    lines.append(f"# Generated: {generated}")
    lines.append(f"# Source hash (sha256[:16]): {src_hash}")
    lines.append(f"# overrides_applied = {overrides_applied} (data/{_OVERRIDES_FILENAME})")
    lines.append(f"# generics_applied = {generics_applied} (comment/disabled universal; place-before/copy-from ordered-only)")
    lines.append("")

    # Hygiene counters — track empty descriptions for quality traceability
    # Upstream leaves ~77% of arguments and ~83% of read-only without description;
    # empty is kept as valid (LSP falls back to name+type) but we trace it.
    total_args = 0
    empty_args = 0
    total_ro = 0
    empty_ro = 0
    resolved_duplicates: list[tuple[str, str, str]] = []

    for menu in menus:
        path = menu["path"]
        menu_type = menu.get("type", "Directory")
        lines.append("[[menus]]")
        lines.append(f'path = "{escape_toml_string(path)}"')
        lines.append(f'type = "{escape_toml_string(menu_type)}"')

        # Flags — deduplicated (first-wins) to avoid TOML weight bloat
        # Type is emitted as single source of truth (additive, not breaking).
        flags = _dedupe_entries(menu.get("flags", []), path, "flag", resolved_duplicates)
        if flags:
            for flag in flags:
                name = escape_toml_string(flag["name"])
                typ = escape_toml_string(clean_type(flag.get("type", "")))
                desc = escape_toml_string(flag.get("description", ""))
                lines.append("[[menus.flags]]")
                lines.append(f'name = "{name}"')
                lines.append(f'type = "{typ}"')
                if desc:
                    lines.append(f'description = "{desc}"')
                if flag.get("required"):
                    lines.append("required = true")

        # Arguments — deduplicated, type escaped, plus missing universal
        # add-item generics (pure: precomputed in generic_map, input unmutated).
        arguments = _dedupe_entries(menu.get("arguments", []), path, "arg", resolved_duplicates)
        arguments = arguments + generic_map.get(path, [])
        total_args += len(arguments)
        empty_args += sum(1 for a in arguments if not a.get("description"))
        if arguments:
            for arg in arguments:
                name = escape_toml_string(arg["name"])
                typ = escape_toml_string(clean_type(arg.get("type", "")))
                desc = escape_toml_string(arg.get("description", ""))
                lines.append("[[menus.arguments]]")
                lines.append(f'name = "{name}"')
                lines.append(f'type = "{typ}"')
                # Enum members are emitted ONLY for writable arguments —
                # flags and read-only values are never user-assigned.
                enum_values = arg.get("enum_values") or []
                if enum_values:
                    rendered = ", ".join(f'"{escape_toml_string(v)}"' for v in enum_values)
                    lines.append(f"enum_values = [{rendered}]")
                if desc:
                    lines.append(f'description = "{desc}"')
                if arg.get("required"):
                    lines.append("required = true")
                if arg.get("unset"):
                    lines.append("unset = true")

        # Read-only arguments — type escaped, deduped
        read_only = _dedupe_entries(menu.get("read_only", []), path, "read_only", resolved_duplicates)
        total_ro += len(read_only)
        empty_ro += sum(1 for a in read_only if not a.get("description"))
        if read_only:
            for arg in read_only:
                name = escape_toml_string(arg["name"])
                typ = escape_toml_string(clean_type(arg.get("type", "")))
                desc = escape_toml_string(arg.get("description", ""))
                lines.append("[[menus.read_only]]")
                lines.append(f'name = "{name}"')
                lines.append(f'type = "{typ}"')
                if desc:
                    lines.append(f'description = "{desc}"')

        lines.append("")

    # Summary trace for upstream duplicate arguments — stderr only
    if resolved_duplicates:
        paths_involved = sorted({p for p, _, _ in resolved_duplicates})
        if len(paths_involved) == 1:
            scope = paths_involved[0]
        elif len(paths_involved) <= 3:
            try:
                common = os.path.commonpath(paths_involved)
                if common and common != "/" and len(common.strip("/").split("/")) >= 2:
                    scope = f"/{common}" if not common.startswith("/") else common
                else:
                    scope = ", ".join(paths_involved)
            except ValueError:
                scope = ", ".join(paths_involved)
        else:
            try:
                common = os.path.commonpath(paths_involved)
                if common and common != "/" and len(common.strip("/").split("/")) >= 2:
                    scope = f"/{common}" if not common.startswith("/") else common
                else:
                    scope = f"{len(paths_involved)} menus ({', '.join(paths_involved[:2])}, ...)"
            except ValueError:
                scope = f"{len(paths_involved)} menus ({', '.join(paths_involved[:2])}, ...)"

        print(
            f"info: {len(resolved_duplicates)} upstream duplicate arguments resolved across {scope}",
            file=sys.stderr,
        )

    # Hygiene trace — stderr only, never pollutes TOML stdout
    if total_args or total_ro:
        pct_args = round(empty_args * 100 / total_args) if total_args else 0
        pct_ro = round(empty_ro * 100 / total_ro) if total_ro else 0
        print(
            f"hygiene: {empty_args}/{total_args} arguments have empty description ({pct_args}%), "
            f"{empty_ro}/{total_ro} read-only empty ({pct_ro}%)",
            file=sys.stderr,
        )

    return "\n".join(lines)


def _strip_generated_line(text: str) -> str:
    """Remove `# Generated:` metadata lines so two outputs that differ only
    by their generation timestamp compare equal."""
    return "\n".join(
        line for line in text.split("\n") if not line.startswith("# Generated:")
    )


def write_if_changed(output_file: Path, new_content: str) -> bool:
    """Write `new_content` to `output_file`, skipping timestamp-only churn.

    If the existing file differs from `new_content` ONLY by its
    `# Generated:` line, the file is left untouched (keeping the old
    timestamp) and False is returned. This keeps `make validate` /
    `git diff --exit-code` clean when nothing material changed, instead of
    dirtying the tracked file with a new timestamp on every run.
    """
    try:
        existing = output_file.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        existing = None

    if existing is not None and _strip_generated_line(existing) == _strip_generated_line(new_content):
        return False

    output_file.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(dir=str(output_file.parent), prefix=output_file.name + ".", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as tmp:
            tmp.write(new_content)
            tmp.flush()
            os.fsync(tmp.fileno())
        os.replace(tmp_name, output_file)
    except BaseException:
        try:
            os.unlink(tmp_name)
        except OSError:
            pass
        raise
    return True


def main():
    script_dir = Path(__file__).parent
    project_root = script_dir.parent if script_dir.name == "scripts" else script_dir
    input_file = project_root / "llms-full.txt"
    output_file = project_root / "data" / "commands.toml"

    if not input_file.exists():
        print(f"Error: {input_file} not found", file=sys.stderr)
        print("Fetch RouterOS docs first:  make sync   (or: python3 scripts/sync_llms.py)", file=sys.stderr)
        sys.exit(1)

    print(f"Parsing {input_file}...")
    # parse_llms_full already applies the should_include() gate to every
    # appended menu, so `menus` contains only CLI-path menus.
    menus = parse_llms_full(str(input_file))
    print(f"Parsed {len(menus)} CLI-path menus.")

    unique = finalize_menus(menus)

    try:
        overrides = load_overrides(project_root / "data" / _OVERRIDES_FILENAME)
    except OverrideError as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)
    applied = apply_overrides(unique, overrides)
    if applied:
        print(f"Applied {applied} curated override(s) from data/{_OVERRIDES_FILENAME}.")

    # Pure preview of generic rows (no mutation): overrides keep precedence
    # because generics only fill names still missing after overrides.
    generics_preview = sum(len(_generic_additions_for_menu(m)) for m in unique)
    if generics_preview:
        print(f"Applied {generics_preview} generic item propertie(s) (universal add-item properties).")

    toml_content = generate_toml(
        unique,
        llms_path=input_file,
        overrides_applied=applied,
        generics_applied=generics_preview,
    )

    if write_if_changed(output_file, toml_content):
        print(f"Wrote {output_file} ({len(unique)} menus)")
    else:
        print(
            f"{output_file} unchanged (only the Generated timestamp differs) — left untouched "
            f"({len(unique)} menus)"
        )


if __name__ == "__main__":
    main()
