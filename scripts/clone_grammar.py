#!/usr/bin/env python3
"""Clone the tree-sitter grammar working copy at the pinned revision.

Single source of truth for the clone-at-rev recipe used by `make
grammar-clone` and the CI/release workflows (previously triplicated as
inline shell). Reads `[grammars.rsc] rev` from extension.toml, validates it,
clones https://github.com/balakar94/tree-sitter-rsc when absent (or reuses an
existing checkout), and detaches HEAD at the pinned commit.

Usage:
  python3 scripts/clone_grammar.py [--dir grammars/rsc] [--rev <sha>]

Exit codes: 0 ok, 1 error, 2 usage (argparse).
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

REPO_URL = "https://github.com/balakar94/tree-sitter-rsc"
REV_RE = re.compile(r"^[0-9a-f]{40}$")


def _project_root() -> Path:
    return Path(__file__).resolve().parent.parent


def read_pinned_rev(root: Path) -> str | None:
    """Parse [grammars.rsc] rev from extension.toml (tomllib, regex fallback)."""
    text = (root / "extension.toml").read_text(encoding="utf-8")
    try:
        import tomllib

        data = tomllib.loads(text)
        rev = data.get("grammars", {}).get("rsc", {}).get("rev")
        return rev.strip() if isinstance(rev, str) else None
    except Exception:
        pass
    in_section = False
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_section = stripped == "[grammars.rsc]"
            continue
        if in_section:
            m = re.match(r"^rev\s*=\s*\"([^\"]+)\"", stripped)
            if m:
                return m.group(1).strip()
    return None


def run_git(args: list[str], cwd: Path) -> str:
    proc = subprocess.run(["git", *args], cwd=str(cwd), capture_output=True, text=True)
    if proc.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)} failed: {proc.stderr.strip()}")
    return proc.stdout.strip()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Clone grammar working copy at the pinned rev")
    parser.add_argument("--dir", default="grammars/rsc", help="Target directory (default: grammars/rsc)")
    parser.add_argument("--rev", default=None, help="Override the pinned rev (default: from extension.toml)")
    args = parser.parse_args(argv)

    root = _project_root()
    target = Path(args.dir) if Path(args.dir).is_absolute() else root / args.dir

    rev = args.rev.strip() if args.rev else read_pinned_rev(root)
    if not rev:
        print("error: [grammars.rsc].rev missing in extension.toml", file=sys.stderr)
        return 1
    if "0000" in rev:
        print(f"error: placeholder rev found ({rev}) — run scripts/publish_grammar.py", file=sys.stderr)
        return 1
    if not REV_RE.match(rev):
        print(f"error: REV must be 40-char hex ({rev})", file=sys.stderr)
        return 1

    marker = target / ".git"
    if target.exists() and not (marker.is_dir() or marker.is_file()):
        print(f"error: {target} exists but is not a git checkout — remove it first", file=sys.stderr)
        return 1
    if not target.exists():
        print(f"cloning {REPO_URL} -> {target}")
        proc = subprocess.run(["git", "clone", "--quiet", REPO_URL, str(target)])
        if proc.returncode != 0:
            print(f"error: git clone failed for {REPO_URL}", file=sys.stderr)
            return 1
    else:
        print(f"{target} already present")

    try:
        head = run_git(["rev-parse", "HEAD"], target)
    except RuntimeError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    if head != rev:
        print(f"fetching pinned rev {rev}")
        try:
            run_git(["fetch", "--quiet", "--depth", "1", "origin", rev], target)
            run_git(["checkout", "--quiet", "--detach", "FETCH_HEAD"], target)
        except RuntimeError as e:
            print(f"error: {e}", file=sys.stderr)
            return 1
    try:
        print(f"grammar pinned at {run_git(['rev-parse', 'HEAD'], target)}")
        print(run_git(["log", "--oneline", "-1"], target))
    except RuntimeError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
