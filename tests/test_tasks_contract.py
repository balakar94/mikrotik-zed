"""QA-F7b: tasks.json <-> scripts contract.

Zed tasks run `python3 scripts/<tool>.py` from `$ZED_WORKTREE_ROOT`. Existing
tests pin task shape, labels and secret hygiene but never connect a task
argument to the script it invokes, so a renamed/moved script or a flag typo
would pass every check. This module closes that gap, offline and fast:

  1. both task files parse and reference `.py` scripts only under `scripts/`
     (no absolute paths, no `..`, no other directories);
  2. every referenced script exists relative to the repo root;
  3. every `--flag` handed to a script is recognized by that script. The
     check is `python3 <script> --help` (run once per script; argparse exits
     before any device/network work), with one documented exception:
     `mikrotik-live-check.py --method` is a deliberate argparse.SUPPRESS
     compatibility shim for tasks.json (`scripts/mikrotik-live-check.py:142`),
     so it is allowlisted below only after verifying the literal exists in
     the script source. The allowlist is staleness-checked: if the shim is
     removed or the flag starts appearing in `--help`, the entry must go.

`tests/test_tasks_mirror.py` already guards byte-identity of the two files;
this module deliberately re-reads both so a divergence in script contracts
cannot hide behind a mirror test failure.
"""

from __future__ import annotations

import json
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
TASK_FILES = (
    ROOT / "languages" / "rsc" / "tasks.json",
    ROOT / ".zed" / "tasks.json",
)

# Flags hidden from a script's `--help` on purpose. Keep this list tiny and
# cite the source; the test verifies the literal still exists in the script
# and that the flag is still absent from help (staleness guard).
SUPPRESSED_FLAGS: dict[tuple[str, str], str] = {
    ("scripts/mikrotik-live-check.py", "--method"): (
        "argparse.SUPPRESS compatibility shim for tasks.json "
        "(scripts/mikrotik-live-check.py:142-143)"
    )
}

_HELP_CACHE: dict[str, str] = {}


def _script_refs(args: list) -> list[str]:
    """Path-like `.py` arguments, in order of appearance.

    Inline `python3 -c` code contains whitespace/newlines and is therefore
    ignored; absolute, relative and bare paths are all detected so the
    "only under scripts/" rule cannot be bypassed.
    """
    refs: list[str] = []
    for arg in args:
        if not isinstance(arg, str) or arg.startswith("-"):
            continue
        if not arg.endswith(".py") or any(c.isspace() for c in arg):
            continue
        refs.append(arg)
    return refs


def _flag_pairs(args: list) -> list[tuple[str, str]]:
    """(script, flag) pairs for flags that follow a script reference."""
    pairs: list[tuple[str, str]] = []
    current: str | None = None
    for arg in args:
        if not isinstance(arg, str):
            continue
        refs = _script_refs([arg])
        if refs:
            current = refs[0]
            continue
        if current is not None and arg.startswith("-") and len(arg) > 1:
            pairs.append((current, arg.split("=", 1)[0]))
    return pairs


def _script_path_violation(ref: str) -> str | None:
    """None when `ref` is a safe repo-relative path under `scripts/`."""
    normalized = ref[2:] if ref.startswith("./") else ref
    if normalized.startswith("/") or pathlib.PurePosixPath(normalized).is_absolute():
        return f"absolute path `{ref}`"
    if ".." in pathlib.PurePosixPath(normalized).parts:
        return f"path traversal `{ref}`"
    if not normalized.startswith("scripts/"):
        return f"path outside scripts/ `{ref}`"
    return None


def _flag_in_help(flag: str, help_text: str) -> bool:
    """Word-boundary match so `--methodx` cannot satisfy `--method`."""
    return re.search(rf"(?<![\w-]){re.escape(flag)}(?![\w-])", help_text) is not None


def _help_text(script: str) -> str:
    """`python3 <script> --help` once per script (offline; no device)."""
    if script not in _HELP_CACHE:
        result = subprocess.run(
            [sys.executable, str(ROOT / script), "--help"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=60,
        )
        assert result.returncode == 0, (
            f"{script} --help exited {result.returncode}:\n{result.stderr}"
        )
        _HELP_CACHE[script] = result.stdout + result.stderr
    return _HELP_CACHE[script]


def _load_tasks() -> list[tuple[pathlib.Path, dict]]:
    loaded: list[tuple[pathlib.Path, dict]] = []
    for path in TASK_FILES:
        tasks = json.loads(path.read_text(encoding="utf-8"))
        assert isinstance(tasks, list) and tasks, f"{path}: no tasks"
        for task in tasks:
            assert isinstance(task, dict), f"{path}: non-object task {task!r}"
            loaded.append((path, task))
    return loaded


# ── Contract over the real task files ────────────────────────────────────


def test_scripts_referenced_by_tasks_exist_under_scripts_dir():
    violations: list[str] = []
    seen: set[str] = set()
    for path, task in _load_tasks():
        label = task.get("label", "?")
        for ref in _script_refs(task.get("args", [])):
            problem = _script_path_violation(ref)
            if problem:
                violations.append(f"{path.name}: {label!r} -> {problem}")
                continue
            normalized = ref[2:] if ref.startswith("./") else ref
            if not (ROOT / normalized).is_file():
                violations.append(f"{path.name}: {label!r} -> missing file `{ref}`")
            seen.add(normalized)
    assert not violations, "task/script path violations:\n" + "\n".join(violations)
    # Guard against a vacuous pass if task files lose their script wiring.
    assert {
        "scripts/mikrotik-deploy.py",
        "scripts/mikrotik-live-check.py",
    } <= seen, f"expected task script references, saw {sorted(seen)}"


def test_task_script_flags_are_recognized_by_scripts():
    violations: list[str] = []
    per_script: dict[str, set[str]] = {}
    for path, task in _load_tasks():
        label = task.get("label", "?")
        for script, flag in _flag_pairs(task.get("args", [])):
            per_script.setdefault(script, set()).add(flag)
            if _flag_in_help(flag, _help_text(script)):
                continue
            if (script, flag) in SUPPRESSED_FLAGS:
                continue
            violations.append(
                f"{path.name}: {label!r} passes {flag} to {script}, not in --help"
            )
    assert not violations, "task/script flag mismatches:\n" + "\n".join(violations)
    # Lock the currently wired flags so a silent parser/association break
    # cannot turn this test into a no-op.
    assert {"--dry-run", "--method"} <= per_script.get(
        "scripts/mikrotik-deploy.py", set()
    ), per_script
    assert {"--host", "--user", "--timeout", "--method", "--dry-run"} <= per_script.get(
        "scripts/mikrotik-live-check.py", set()
    ), per_script


def test_suppressed_flag_allowlist_is_real_and_not_stale():
    for (script, flag), reason in SUPPRESSED_FLAGS.items():
        help_text = _help_text(script)
        assert not _flag_in_help(flag, help_text), (
            f"allowlist entry stale: {flag} now appears in {script} --help — "
            f"remove it from SUPPRESSED_FLAGS ({reason})"
        )
        source = (ROOT / script).read_text(encoding="utf-8")
        assert flag in source, (
            f"allowlist entry stale: {flag} not found in {script} source — "
            f"the compatibility shim was removed ({reason})"
        )


# ── Self-tests: the gate must be able to fail ────────────────────────────


def test_script_ref_detection_rejects_paths_outside_scripts():
    refs = _script_refs(
        ["./scripts/ok.py", "--flag", "/tmp/evil.py", "../evil.py", "evil.py", "-c", "import x"]
    )
    assert refs == ["./scripts/ok.py", "/tmp/evil.py", "../evil.py", "evil.py"]
    assert _script_path_violation("./scripts/ok.py") is None
    assert "absolute" in (_script_path_violation("/tmp/evil.py") or "")
    assert "traversal" in (_script_path_violation("../evil.py") or "")
    assert "outside scripts/" in (_script_path_violation("evil.py") or "")


def test_flag_matching_rejects_typos():
    help_text = "usage: tool [--method {rest,ssh}] [--dry-run]\n"
    assert _flag_in_help("--method", help_text)
    assert _flag_in_help("--dry-run", help_text)
    assert not _flag_in_help("--methodx", help_text)
    assert not _flag_in_help("--dryrun", help_text)
    assert _flag_pairs(["scripts/x.py", "--known", "rest", "$ZED_FILE"]) == [
        ("scripts/x.py", "--known")
    ]
    # Python interpreter flags before any script reference are not attributed.
    assert _flag_pairs(["-c", "code", "scripts/x.py", "--after"]) == [
        ("scripts/x.py", "--after")
    ]
