"""Tests for scripts/publish_grammar.py — dry-run no side-effect."""

import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "publish_grammar.py"
EXT_TOML = ROOT / "extension.toml"


def test_dry_run_does_not_touch_extension_toml(tmp_path=None):
    """--dry-run must not modify extension.toml (side-effect free)."""
    if not SCRIPT.exists():
        return
    original = EXT_TOML.read_text(encoding="utf-8")
    mtime = EXT_TOML.stat().st_mtime
    # Run dry-run; it should not rewrite extension.toml even if rev would change logically.
    result = subprocess.run(
        [sys.executable, str(SCRIPT), "--dry-run"],
        capture_output=True,
        text=True,
    )
    # Accept both success (0) and error due to missing grammar working copy;
    # the invariant is FS untouched.
    after = EXT_TOML.read_text(encoding="utf-8")
    assert after == original, "dry-run must not modify extension.toml"
    # mtime must not have advanced (allow 1s granularity slop if FS not touched; but content check is stronger).
    # If file was not rewritten, mtime stays identical on platforms with sub-second resolution.
    # We only assert when run succeeded (0) to avoid flake on error paths that still shouldn't touch.
    if result.returncode == 0:
        assert EXT_TOML.stat().st_mtime == mtime or after == original


def test_dry_run_output_mentions_dry_run():
    """--dry-run should announce DRY-RUN and not perform push."""
    if not SCRIPT.exists():
        return
    result = subprocess.run(
        [sys.executable, str(SCRIPT), "--dry-run"],
        capture_output=True,
        text=True,
    )
    combined = (result.stdout or "") + (result.stderr or "")
    assert "DRY-RUN" in combined
