#!/usr/bin/env python3
"""
Sync llms.txt and llms-full.txt from Mikrotik documentation.

Fetches fresh copies from https://manual.mikrotik.com/llms.txt and
https://manual.mikrotik.com/llms-full.txt, with diff check and version bump logic.

Usage:
  python3 scripts/sync_llms.py [--check] [--force]

Options:
  --check  Dry-run: fetch and show diff stats without writing files.
  --force  Force overwrite even if content unchanged.

Behavior:
  - Fetches both files via urllib with timeout and retries.
  - Computes SHA-256 hash and compares to local files.
  - Extracts RouterOS version hints from llms-full.txt header for logging.
  - If changed, overwrites local files and prints version diff.
  - Exits 0 on success, 1 on fetch failure, 2 if --check detects pending changes.
"""

import argparse
import hashlib
import re
import sys
import urllib.error
import urllib.request
from pathlib import Path

BASE_URL = "https://manual.mikrotik.com"
FILES = {
    "llms.txt": f"{BASE_URL}/llms.txt",
    "llms-full.txt": f"{BASE_URL}/llms-full.txt",
}

TIMEOUT = 30  # seconds
RETRIES = 3


def fetch_url(url: str) -> bytes:
    """Fetch URL with retries, timeout, and proper headers."""
    last_exc = None
    for attempt in range(1, RETRIES + 1):
        try:
            req = urllib.request.Request(
                url,
                headers={
                    "User-Agent": "mikrotik-zed sync_llms.py/1.0",
                    "Accept": "text/plain, text/markdown, */*",
                },
            )
            with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
                if resp.status != 200:
                    raise urllib.error.HTTPError(url, resp.status, f"HTTP {resp.status}", resp.headers, None)
                return resp.read()
        except Exception as e:
            last_exc = e
            print(f"Attempt {attempt}/{RETRIES} failed for {url}: {e}", file=sys.stderr)
            if attempt < RETRIES:
                continue
    raise last_exc  # type: ignore[misc]


def sha256(data: bytes) -> str:
    h = hashlib.sha256()
    h.update(data)
    return h.hexdigest()


def sha256_file(path: Path) -> str | None:
    if not path.exists():
        return None
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(8192), b""):
            h.update(chunk)
    return h.hexdigest()


def extract_version(text: str) -> str:
    """Extract RouterOS version hint from text (first 8 KiB)."""
    head = text[:8192]
    m = re.search(r"RouterOS\s+v?(\d+\.\d+(?:\.\d+)?)", head, re.IGNORECASE)
    if m:
        return m.group(1)
    m = re.search(r"\b7\.\d+(?:\.\d+)?\b", head)
    if m:
        return m.group(0)
    return "unknown"


def main() -> int:
    parser = argparse.ArgumentParser(description="Sync llms.txt and llms-full.txt from manual.mikrotik.com")
    parser.add_argument("--check", action="store_true", help="Dry-run diff check without writing")
    parser.add_argument("--force", action="store_true", help="Force overwrite even if unchanged")
    args = parser.parse_args()

    script_dir = Path(__file__).parent
    project_root = script_dir.parent if script_dir.name == "scripts" else script_dir

    overall_changed = False
    has_error = False

    for filename, url in FILES.items():
        local_path = project_root / filename
        print(f"Fetching {url} ...")
        try:
            data = fetch_url(url)
        except Exception as e:
            print(f"ERROR: failed to fetch {url}: {e}", file=sys.stderr)
            has_error = True
            continue

        remote_hash = sha256(data)
        local_hash = sha256_file(local_path)
        remote_text = data.decode("utf-8", errors="ignore")
        remote_version = extract_version(remote_text)

        if local_hash is not None:
            local_version = extract_version(local_path.read_text(encoding="utf-8", errors="ignore"))
        else:
            local_version = "missing"

        if local_hash == remote_hash and not args.force:
            print(f"  {filename}: unchanged (hash {remote_hash[:16]}, version {remote_version})")
            continue

        # Diff stats
        if local_path.exists():
            local_lines = local_path.read_text(encoding="utf-8", errors="ignore").splitlines()
            remote_lines = remote_text.splitlines()
            added = len(remote_lines) - len(local_lines)
            print(f"  {filename}: changed (local {local_hash[:16] if local_hash else 'missing'} -> remote {remote_hash[:16]})")
            print(f"    version: {local_version} -> {remote_version}")
            print(f"    lines: {len(local_lines)} -> {len(remote_lines)} ({'+' if added >=0 else ''}{added})")
            # Simple diff bump hint: count headings
            local_headings = sum(1 for l in local_lines if l.startswith("## "))
            remote_headings = sum(1 for l in remote_lines if l.startswith("## "))
            print(f"    headings (##): {local_headings} -> {remote_headings} ({remote_headings - local_headings:+d})")
        else:
            print(f"  {filename}: new file (remote hash {remote_hash[:16]}, version {remote_version})")

        overall_changed = True

        if args.check:
            print(f"  {filename}: --check mode, not writing")
            continue

        # Write file
        try:
            local_path.write_bytes(data)
            print(f"  {filename}: wrote {len(data)} bytes")
        except Exception as e:
            print(f"ERROR: failed to write {local_path}: {e}", file=sys.stderr)
            has_error = True

    if has_error:
        return 1
    if args.check and overall_changed:
        print("Check: updates available (run without --check to apply)", file=sys.stderr)
        return 2
    if overall_changed:
        print("Sync complete: files updated. Run `python3 scripts/extract_commands.py` to regenerate commands.toml.")
    else:
        print("Sync complete: no changes.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
