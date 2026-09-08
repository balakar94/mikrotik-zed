#!/usr/bin/env python3
"""Bump the extension release version across the release files.

Replaces the fragile single-shot regex (`re.sub(..., count=1)`, which
silently edits whatever `version = ...` line happens to come first) with a
section-aware edit verified by `tomllib`:

- `Cargo.toml` / `lsp/Cargo.toml`: only the `version` line inside the
  `[package]` section is rewritten (dependency `version = "1"` requirements
  elsewhere in the file are never touched).
- `extension.toml`: only the root-level `version` line is rewritten
  (`[grammars.rsc] rev` and friends are never touched).

After editing, every file is re-parsed with `tomllib` and the new version is
asserted, then `cargo check` refreshes `Cargo.lock` and the lock is asserted
to carry the new version for the workspace members. Grammar versions
(`grammars/rsc/`, separate repo lifecycle) are never touched here.

Usage: python3 scripts/bump_version.py <VERSION> <file> [<file> ...]
"""

from __future__ import annotations

import pathlib
import re
import subprocess
import sys

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11
    print("error: bump_version.py needs Python 3.11+ (tomllib)", file=sys.stderr)
    sys.exit(2)

ROOT = pathlib.Path(__file__).resolve().parent.parent

SEMVER_RE = re.compile(r"^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$")
SECTION_RE = re.compile(r"^\s*\[(?P<section>[^\]]+)\]\s*(?:#.*)?$")
VERSION_LINE_RE = re.compile(
    r'^(?P<indent>\s*)version\s*=\s*"(?P<old>[^"]*)"(?P<rest>\s*(?:#.*)?)$'
)

# Workspace members whose versions must agree with Cargo.lock after refresh.
LOCK_MEMBERS = ("mikrotik-zed", "rsc-ls")


def target_section(path: pathlib.Path) -> str | None:
    """TOML section holding the release version: `[package]` for manifests
    named Cargo.toml, the document root for everything else (extension.toml)."""
    if path.name == "Cargo.toml":
        return "package"
    return None


def toml_key(path: pathlib.Path) -> tuple[str, ...]:
    if path.name == "Cargo.toml":
        return ("package", "version")
    return ("version",)


def bump_file(path: pathlib.Path, version: str) -> None:
    section = target_section(path)
    lines = path.read_text(encoding="utf-8").splitlines(keepends=True)
    current: str | None = None
    edits = 0
    out: list[str] = []
    for line in lines:
        section_match = SECTION_RE.match(line.rstrip("\r\n"))
        if section_match:
            current = section_match.group("section").strip()
            out.append(line)
            continue
        if current == section:
            version_match = VERSION_LINE_RE.match(line.rstrip("\r\n"))
            if version_match:
                ending = "\r\n" if line.endswith("\r\n") else ("\n" if line.endswith("\n") else "")
                rebuilt = (
                    f"{version_match.group('indent')}version = "
                    f'"{version}"{version_match.group("rest")}{ending}'
                )
                out.append(rebuilt)
                edits += 1
                continue
        out.append(line)
    if edits != 1:
        print(
            f"error: expected exactly 1 version edit in section "
            f"[{section or '<root>'}] of {path}, made {edits}",
            file=sys.stderr,
        )
        sys.exit(1)
    path.write_text("".join(out), encoding="utf-8")


def verify_file(path: pathlib.Path, version: str) -> None:
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    node: object = data
    for key in toml_key(path):
        if not isinstance(node, dict) or key not in node:
            print(f"error: {path} missing key {'.'.join(toml_key(path))}", file=sys.stderr)
            sys.exit(1)
        node = node[key]
    if node != version:
        print(
            f"error: {path} version is {node!r}, expected {version!r} "
            f"(edit did not land where tomllib reads it)",
            file=sys.stderr,
        )
        sys.exit(1)


def refresh_and_assert_lock(version: str) -> None:
    """Run `cargo check` so Cargo.lock refreshes, then assert the lock records
    the new version for every workspace member (fails loudly instead of
    leaving a stale lock to be committed)."""
    # A plain check refreshes Cargo.lock (the lock legitimately moves after a
    # version bump, so `--locked` would wrongly fail here); the assertion
    # below then proves the refreshed lock carries the new version.
    try:
        result = subprocess.run(
            ["cargo", "check", "--workspace"],
            cwd=ROOT,
            capture_output=True,
            text=True,
        )
    except FileNotFoundError:
        print("error: cargo not found — cannot refresh/assert Cargo.lock", file=sys.stderr)
        sys.exit(1)
    if result.returncode != 0:
        print(result.stdout[-2000:], file=sys.stderr)
        print(result.stderr[-2000:], file=sys.stderr)
        print("error: cargo check failed — Cargo.lock not refreshed", file=sys.stderr)
        sys.exit(1)
    lock_text = (ROOT / "Cargo.lock").read_text(encoding="utf-8")
    blocks = lock_text.split("[[package]]")
    missing = []
    for member in LOCK_MEMBERS:
        ok = any(
            re.search(rf'^name\s*=\s*"{re.escape(member)}"$', block, re.MULTILINE)
            and re.search(rf'^version\s*=\s*"{re.escape(version)}"$', block, re.MULTILINE)
            for block in blocks
        )
        if not ok:
            missing.append(member)
    if missing:
        print(
            f"error: Cargo.lock lacks version {version!r} for: {', '.join(missing)} "
            f"(commit would ship a stale lock)",
            file=sys.stderr,
        )
        sys.exit(1)


def main(argv: list[str]) -> int:
    if len(argv) < 3:
        print(f"usage: {argv[0]} <VERSION> <file> [<file> ...]", file=sys.stderr)
        return 2
    version = argv[1]
    if not SEMVER_RE.match(version):
        print(f"error: VERSION must be semver x.y.z, got {version!r}", file=sys.stderr)
        return 2
    files = [pathlib.Path(arg) for arg in argv[2:]]
    for path in files:
        if not path.is_file():
            print(f"error: file not found: {path}", file=sys.stderr)
            return 2
    for path in files:
        before = tomllib.loads(path.read_text(encoding="utf-8"))
        _ = before  # parsed to fail fast on invalid TOML before editing
        bump_file(path, version)
    for path in files:
        verify_file(path, version)
        print(f"bumped {path} to {version}")
    refresh_and_assert_lock(version)
    print(f"Cargo.lock refreshed and asserts version {version}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
