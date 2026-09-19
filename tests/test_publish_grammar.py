"""Tests for scripts/publish_grammar.py — dry-run purity, --branch, fail-closed gates."""

import os
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "publish_grammar.py"
EXT_TOML = ROOT / "extension.toml"
GRAMMAR_DIR = ROOT / "grammars" / "rsc"


def _grammar_ready() -> bool:
    """True when the untracked grammar working copy can actually be published."""
    return (GRAMMAR_DIR / ".git").exists() and (GRAMMAR_DIR / "grammar.js").is_file()


def _has_origin_remote() -> bool:
    result = subprocess.run(
        ["git", "-C", str(GRAMMAR_DIR), "remote"],
        capture_output=True,
        text=True,
    )
    return "origin" in result.stdout.split()


def _grammar_head() -> str:
    result = subprocess.run(
        ["git", "-C", str(GRAMMAR_DIR), "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout.strip()


def test_dry_run_does_not_touch_extension_toml():
    """--dry-run must not modify extension.toml (side-effect free)."""
    if not SCRIPT.exists():
        pytest.skip("scripts/publish_grammar.py absent")
    original = EXT_TOML.read_text(encoding="utf-8")
    mtime = EXT_TOML.stat().st_mtime
    result = subprocess.run(
        [sys.executable, str(SCRIPT), "--dry-run"],
        capture_output=True,
        text=True,
    )
    # Accept both success (0) and error due to missing grammar working copy;
    # the invariant is FS untouched.
    after = EXT_TOML.read_text(encoding="utf-8")
    assert after == original, "dry-run must not modify extension.toml"
    if result.returncode == 0:
        assert EXT_TOML.stat().st_mtime == mtime or after == original


def test_dry_run_output_mentions_dry_run():
    """--dry-run should announce DRY-RUN and not perform push."""
    if not SCRIPT.exists():
        pytest.skip("scripts/publish_grammar.py absent")
    result = subprocess.run(
        [sys.executable, str(SCRIPT), "--dry-run"],
        capture_output=True,
        text=True,
    )
    combined = (result.stdout or "") + (result.stderr or "")
    assert "DRY-RUN" in combined


def test_help_documents_branch_option():
    """--branch must be documented with its main default."""
    result = subprocess.run(
        [sys.executable, str(SCRIPT), "--help"],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0
    combined = result.stdout + result.stderr
    assert "--branch" in combined
    assert "default: main" in combined


def test_dry_run_targets_requested_branch():
    """--dry-run must name the push target (<remote> HEAD:<branch>) without pushing."""
    if not GRAMMAR_DIR.exists():
        pytest.skip("grammars/rsc absent (run 'make grammar-clone')")
    result = subprocess.run(
        [sys.executable, str(SCRIPT), "--dry-run", "--branch", "release/0.7.0"],
        capture_output=True,
        text=True,
    )
    combined = (result.stdout or "") + (result.stderr or "")
    assert result.returncode == 0, f"dry-run failed: {combined}"
    assert "DRY-RUN" in combined
    assert "HEAD:release/0.7.0" in combined, f"branch target missing from output: {combined}"


def test_fail_closed_when_generate_fails(tmp_path):
    """A failing tree-sitter generate must abort before any commit or push.

    A fake `npx` on PATH exits 1; the script must fail non-zero, leave
    extension.toml untouched and leave the grammar HEAD where it was.
    """
    if not _grammar_ready():
        pytest.skip("grammars/rsc not a git checkout (run 'make grammar-clone')")
    if not _has_origin_remote():
        pytest.skip("grammars/rsc has no origin remote (cannot publish safely)")

    original_ext = EXT_TOML.read_text(encoding="utf-8")
    original_head = _grammar_head()

    fake_npx = tmp_path / "npx"
    fake_npx.write_text("#!/bin/sh\necho 'fake npx: generate failed' >&2\nexit 1\n", encoding="utf-8")
    fake_npx.chmod(0o755)

    env = dict(os.environ)
    env["PATH"] = f"{tmp_path}{os.pathsep}{env.get('PATH', '')}"
    result = subprocess.run(
        [sys.executable, str(SCRIPT)],
        capture_output=True,
        text=True,
        env=env,
    )

    combined = (result.stdout or "") + (result.stderr or "")
    assert result.returncode != 0, f"publish must fail closed, got rc=0: {combined}"
    assert "tree-sitter generate" in combined, f"failure must name the generate gate: {combined}"
    assert EXT_TOML.read_text(encoding="utf-8") == original_ext, "extension.toml must be untouched"
    assert _grammar_head() == original_head, "grammar HEAD must be untouched"


def test_source_keeps_fail_closed_gates():
    """Static guard: the generate/corpus/diff gates must not be silently dropped."""
    source = SCRIPT.read_text(encoding="utf-8")
    assert '_run_npx(["tree-sitter", "generate"], "generate")' in source, "generate gate missing"
    assert '_run_npx(["tree-sitter", "test"], "test")' in source, "corpus gate missing"
    assert '"--exit-code"' in source, "generate freshness diff gate missing"
