"""QA-F7a: negative fixtures for the docs gate (`scripts/check_docs.py`).

A quality gate that is never fed a violating input can rot into a no-op
without any CI signal. These tests import the gate module, point its
`REPO_ROOT` at a temporary docs tree, and prove each class of violation is
reported with a non-zero exit code:

  - V1 volatile literal (pasted version / SHA / snapshots)
  - L1 broken relative link, broken same-file anchor
  - L2 absolute `/docs/...` link
  - R1 page not reachable from `docs/index.md`

They also assert a clean temporary tree passes (no false positives) and
that the real `docs/` tree stays clean via the actual CLI entrypoint.
`scripts/check_docs.py` is never modified.
"""

from __future__ import annotations

import importlib.util
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
CHECK_DOCS = ROOT / "scripts" / "check_docs.py"

CLEAN_INDEX = "# Index\n\n- [Page](page.md)\n"
CLEAN_PAGE = "# Page\n\nBody text.\n"


def _load_gate():
    spec = importlib.util.spec_from_file_location("_docs_gate_under_test", CHECK_DOCS)
    assert spec and spec.loader, "could not load scripts/check_docs.py"
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _run_gate(monkeypatch, tmp_path: pathlib.Path, files: dict[str, str]) -> int:
    """Write `files` under tmp_path/docs and run the gate against them."""
    docs = tmp_path / "docs"
    docs.mkdir(parents=True, exist_ok=True)
    for name, text in files.items():
        (docs / name).write_text(text, encoding="utf-8")
    gate = _load_gate()
    # check_links/check_volatile/check_reachability resolve paths against
    # REPO_ROOT; point it at the fixture root so tmp files stay "in repo".
    monkeypatch.setattr(gate, "REPO_ROOT", tmp_path)
    monkeypatch.setattr(sys, "argv", ["check_docs.py", "--docs", str(docs)])
    return gate.main()


def test_clean_tree_passes(tmp_path, monkeypatch, capsys):
    code = _run_gate(
        monkeypatch, tmp_path, {"index.md": CLEAN_INDEX, "page.md": CLEAN_PAGE}
    )
    out = capsys.readouterr().out
    assert code == 0, out
    assert "0 problems" in out, out


def test_volatile_literal_fails(tmp_path, monkeypatch, capsys):
    page = "# Page\n\nPinned against 9.9.9 today.\n"
    code = _run_gate(monkeypatch, tmp_path, {"index.md": CLEAN_INDEX, "page.md": page})
    out = capsys.readouterr().out
    assert code == 1, out
    assert "pasted volatile literal `9.9.9`" in out, out


def test_broken_link_anchor_and_absolute_link_fail(tmp_path, monkeypatch, capsys):
    page = (
        "# Page\n\n"
        "See [missing](nope.md), [bad](#no-such-heading) "
        "and [absolute](/docs/index.md).\n"
    )
    code = _run_gate(monkeypatch, tmp_path, {"index.md": CLEAN_INDEX, "page.md": page})
    out = capsys.readouterr().out
    assert code == 1, out
    assert "link target not found `nope.md`" in out, out
    assert "anchor `#no-such-heading` not found in file" in out, out
    assert "absolute link `/docs/index.md`" in out, out


def test_unreachable_page_fails(tmp_path, monkeypatch, capsys):
    files = {
        "index.md": CLEAN_INDEX,
        "page.md": CLEAN_PAGE,
        "orphan.md": "# Orphan\n\nNo inbound link.\n",
    }
    code = _run_gate(monkeypatch, tmp_path, files)
    out = capsys.readouterr().out
    assert code == 1, out
    assert "orphan.md: not reachable from docs/index.md" in out, out


def test_real_docs_tree_stays_clean():
    """The shipped docs must pass the real CLI exactly as `make docs-check` runs it."""
    result = subprocess.run(
        [sys.executable, str(CHECK_DOCS)],
        cwd=ROOT,
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert "0 problems" in result.stdout, result.stdout
