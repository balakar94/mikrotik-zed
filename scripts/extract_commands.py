#!/usr/bin/env python3
"""
Extract RouterOS CLI command data from llms-full.txt and generate commands.toml.

Parses the CLI Reference section of llms-full.txt to extract menu paths,
argument names, types, descriptions, and flags for the COMPLETE RouterOS
CLI surface (~1017 menus). All roots are discovered via the CLI-path regex
(^[a-z0-9][a-z0-9/_-]*$) rather than a hardcoded whitelist.

Output: data/commands.toml (TOML format for the Zed language server)
"""

import hashlib
import re
import sys
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

def _extract_heading_path(line: str) -> str | None:
    """Extract a RouterOS menu path from a markdown heading, or None.

    Handles ##, ###, #### headings; strips markdown links and trailing dots;
    returns None for non-menu headings (no '/' or starts with '#').
    """
    m = HEADING_RE.match(line)
    if not m:
        return None
    path = m.group(1).strip()
    path = re.sub(r"\[.*?\]\(.*?\)", "", path).strip()
    path = path.rstrip(".")
    if "/" in path and not path.startswith("#"):
        return path
    return None


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


def parse_llms_full(filepath: str) -> list[dict]:
    """Parse llms-full.txt and extract menu entries."""
    with open(filepath, "r", encoding="utf-8") as f:
        content = f.read()

    menus = []
    current_menu = None
    current_section = None  # "flags", "arguments", or "readonly"
    in_argtable = False

    lines = content.split("\n")

    for i, line in enumerate(lines):
        # Detect menu path from ##, ###, or #### headings containing "/"
        heading_path = _extract_heading_path(line)
        if heading_path is not None:
            # Save previous menu if it exists and should be included
            if current_menu and should_include(current_menu["path"]):
                menus.append(current_menu)

            current_menu = {
                "path": "/" + heading_path,  # Add leading /
                "type": "Directory",
                "flags": [],
                "arguments": [],
                "read_only": [],
            }
            current_section = None
            in_argtable = False
            continue

        # Detect Type
        type_match = re.match(r"^\*\*Type:\*\*\s+(.+)", line)
        if type_match and current_menu:
            current_menu["type"] = type_match.group(1).strip()
            continue

        # Detect ArgTable end first (before start, since </ArgTable> also contains <ArgTable)
        if "</ArgTable>" in line:
            in_argtable = False
            current_section = None
            continue

        # Detect ArgTableRow (before ArgTable, since <ArgTableRow contains <ArgTable)
        if in_argtable and current_menu and "<ArgTableRow" in line:
            arg_match = re.search(r'arg="([^"]+)"', line)

            # Anonymous row (no arg attribute or empty name): skip entirely
            # instead of emitting entries with an empty identifier.
            if arg_match is None or not arg_match.group(1):
                continue

            typ_match = re.search(r'typ="([^"]*)"', line)
            mandatory_match = re.search(r'mandatory="1"', line)
            unset_match = re.search(r'unset="1"', line)

            # Extract description (text between > and </ArgTableRow>)
            desc_match = re.search(r">([^<]*)</ArgTableRow", line)
            description = desc_match.group(1).strip() if desc_match else ""

            entry = {
                "name": arg_match.group(1),
                "type": typ_match.group(1) if typ_match else "",
                "required": bool(mandatory_match),
                "unset": bool(unset_match),
                "description": description,
            }

            # Preserve enum members from the RAW type string. The `type`
            # field is truncated by clean_type() at emit time, which would
            # otherwise make downstream enum parsing lose members (or fail
            # entirely on the trailing "..." of long enums).
            enum_values = extract_enum_values(entry["type"])
            if enum_values:
                entry["enum_values"] = enum_values

            if current_section == "flags":
                current_menu["flags"].append(entry)
            elif current_section == "arguments":
                current_menu["arguments"].append(entry)
            elif current_section == "readonly":
                current_menu["read_only"].append(entry)
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
            continue

    # Save last menu
    if current_menu and should_include(current_menu["path"]):
        menus.append(current_menu)

    return menus


def clean_type(typ: str) -> str:
    """Clean and simplify type strings for the TOML output."""
    # Remove excessive whitespace
    typ = re.sub(r"\s+", " ", typ).strip()
    # Truncate very long type descriptions
    if len(typ) > 100:
        typ = typ[:97] + "..."
    return typ


def escape_toml_string(s: str) -> str:
    """Escape a string for TOML literal string representation."""
    # Replace backslashes and quotes
    s = s.replace("\\", "\\\\")
    s = s.replace('"', '\\"')
    # Remove newlines from descriptions
    s = s.replace("\n", " ").replace("\r", "")
    return s


def _extract_routeros_version(llms_path: Path) -> str:
    """Extract RouterOS version string from llms-full.txt header if available."""
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


def generate_toml(menus: list[dict], llms_path: Path | None = None) -> str:
    """Generate TOML output from parsed menus."""
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
    lines.append("")

    for menu in menus:
        path = menu["path"]
        menu_type = menu["type"]
        lines.append("[[menus]]")
        lines.append(f'path = "{escape_toml_string(path)}"')
        lines.append(f'type = "{escape_toml_string(menu_type)}"')

        # Flags
        if menu["flags"]:
            for flag in menu["flags"]:
                name = escape_toml_string(flag["name"])
                desc = escape_toml_string(flag["description"])
                lines.append("[[menus.flags]]")
                lines.append(f'name = "{name}"')
                lines.append(f'description = "{desc}"')
                if flag.get("required"):
                    lines.append("required = true")

        # Arguments
        if menu["arguments"]:
            for arg in menu["arguments"]:
                name = escape_toml_string(arg["name"])
                typ = clean_type(arg["type"])
                desc = escape_toml_string(arg["description"])
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

        # Read-only arguments
        if menu["read_only"]:
            for arg in menu["read_only"]:
                name = escape_toml_string(arg["name"])
                typ = clean_type(arg["type"])
                desc = escape_toml_string(arg["description"])
                lines.append("[[menus.read_only]]")
                lines.append(f'name = "{name}"')
                lines.append(f'type = "{typ}"')
                if desc:
                    lines.append(f'description = "{desc}"')

        lines.append("")

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
    output_file.write_text(new_content, encoding="utf-8")
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

    toml_content = generate_toml(unique, llms_path=input_file)

    if write_if_changed(output_file, toml_content):
        print(f"Wrote {output_file} ({len(unique)} menus)")
    else:
        print(
            f"{output_file} unchanged (only the Generated timestamp differs) — left untouched "
            f"({len(unique)} menus)"
        )


if __name__ == "__main__":
    main()
