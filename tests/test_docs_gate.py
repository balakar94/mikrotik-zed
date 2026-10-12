"""QA-F7a: negative fixtures for the docs gate (`scripts/check_docs.py`).

A quality gate that is never fed a violating input can rot into a no-op
without any CI signal. These tests import the gate module, point its
`REPO_ROOT` at a temporary docs tree, and prove each class of violation is
reported with a non-zero exit code:

  - V1 volatile literal (pasted version / SHA / snapshots)
  - L1 broken relative link, broken same-file anchor
  - L2 absolute `/docs/...` link
  - R1 page not reachable from `docs/index.md`
  - recursion: nested pages (`docs/**/*.md`) are scanned, with the
    directory/file allowlists still applying

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
    """Write `files` under tmp_path/docs and run the gate against them.

    Keys may be nested paths (``sub/page.md``); parent directories are created.
    """
    docs = tmp_path / "docs"
    docs.mkdir(parents=True, exist_ok=True)
    for name, text in files.items():
        target = docs / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")
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


def test_nested_page_is_scanned(tmp_path, monkeypatch, capsys):
    """`docs/**/*.md`: a violation under a subdirectory must be reported."""
    files = {
        "index.md": CLEAN_INDEX,
        "page.md": CLEAN_PAGE,
        "export-fixtures/README.md": "# Exports\n\nPinned against 9.9.9 today.  \n",
    }
    code = _run_gate(monkeypatch, tmp_path, files)
    out = capsys.readouterr().out
    assert code == 1, out
    assert "export-fixtures/README.md:3: trailing whitespace" in out, out
    assert "pasted volatile literal `9.9.9`" in out, out


def test_nested_page_counted_in_summary(tmp_path, monkeypatch, capsys):
    """The summary counts nested pages, not only top-level ones."""
    files = {
        "index.md": CLEAN_INDEX,
        "page.md": CLEAN_PAGE,
        "adr/0001-note.md": "# Note\n\nBody.\n",
    }
    code = _run_gate(monkeypatch, tmp_path, files)
    out = capsys.readouterr().out
    assert code == 0, out
    assert "docs-check: 3 files, 0 problems" in out, out


def test_allowlisted_dirs_skip_reachability_at_depth(tmp_path, monkeypatch, capsys):
    """`adr/` and `export-fixtures/` stay exempt from R1 when nested."""
    files = {
        "index.md": CLEAN_INDEX,
        "page.md": CLEAN_PAGE,
        "adr/0001-note.md": "# Note\n\nBody.\n",
        "export-fixtures/README.md": "# Exports\n\nBody.\n",
    }
    code = _run_gate(monkeypatch, tmp_path, files)
    out = capsys.readouterr().out
    assert code == 0, out
    assert "not reachable from docs/index.md" not in out, out


def test_allowlisted_file_still_requires_index_link(tmp_path, monkeypatch, capsys):
    """The file allowlist is path-scoped: top level only, index link required."""
    files = {
        "index.md": CLEAN_INDEX,
        "page.md": CLEAN_PAGE,
        "publishing-runbook.md": "# Runbook\n\nBody.\n",
    }
    code = _run_gate(monkeypatch, tmp_path, files)
    out = capsys.readouterr().out
    assert code == 1, out
    assert "publishing-runbook.md: not linked from docs/index.md" in out, out

    linked = dict(files)
    linked["index.md"] = CLEAN_INDEX + "- [Runbook](publishing-runbook.md)\n"
    code = _run_gate(monkeypatch, tmp_path, linked)
    out = capsys.readouterr().out
    assert code == 0, out


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


def test_docs_dir_outside_repo_root_is_a_usage_error(tmp_path, monkeypatch, capsys):
    """`--docs` outside REPO_ROOT must fail with a message, not a traceback.

    Every rule resolves paths against REPO_ROOT, so an out-of-tree docs dir
    used to raise `ValueError` from `relative_to` deep inside `check_volatile`
    (and would have reported every link as escaping the repo). It is a usage
    error: exit 2, no traceback.
    """
    repo = tmp_path / "repo"
    (repo / "docs").mkdir(parents=True)
    outside = tmp_path / "outside"
    (outside / "docs").mkdir(parents=True)
    (outside / "docs" / "index.md").write_text(CLEAN_INDEX, encoding="utf-8")

    gate = _load_gate()
    monkeypatch.setattr(gate, "REPO_ROOT", repo)
    monkeypatch.setattr(sys, "argv", ["check_docs.py", "--docs", str(outside / "docs")])

    assert gate.main() == 2
    captured = capsys.readouterr()
    assert "must be inside the repo root" in captured.err, captured.err
    assert "Traceback" not in captured.err
