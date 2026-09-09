"""
Tasks mirror gate: .zed/tasks.json <-> languages/rsc/tasks.json.

The repo ships the same 6 Zed tasks twice: `.zed/tasks.json` (local dev)
and `languages/rsc/tasks.json` (packaged with the extension). They must
stay byte-identical so local runs match what reviewers/users get.
Secret hygiene: no task may store MIKROTIK_PASS (or any password) in `env`;
credentials travel via process env/keychain only. The enable-hint task may
name the variable in guidance text (args) — that is documentation, not storage.

Deterministic, no network, fast.
"""
import hashlib
import json
import pathlib

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
ZED_TASKS = REPO_ROOT / ".zed" / "tasks.json"
LANG_TASKS = REPO_ROOT / "languages" / "rsc" / "tasks.json"

EXPECTED_COUNT = 6


def _read(path: pathlib.Path) -> bytes:
    return path.read_bytes()


def _labels(tasks: list) -> list:
    return [t.get("label") for t in tasks]


def test_tasks_files_byte_identical():
    a = _read(ZED_TASKS)
    b = _read(LANG_TASKS)
    ha = hashlib.sha256(a).hexdigest()
    hb = hashlib.sha256(b).hexdigest()
    assert ha == hb, (
        f"tasks mirror diverged: .zed/tasks.json sha {ha[:12]} vs "
        f"languages/rsc/tasks.json sha {hb[:12]} — "
        "edit one, then copy to the other so both stay byte-identical"
    )


def test_tasks_label_parity_and_count():
    zed = json.loads(_read(ZED_TASKS).decode("utf-8"))
    lang = json.loads(_read(LANG_TASKS).decode("utf-8"))
    assert _labels(zed) == _labels(lang), (
        f"label parity failed:\n zed={_labels(zed)}\n lang={_labels(lang)}"
    )
    assert len(zed) == len(lang) == EXPECTED_COUNT, (
        f"expected {EXPECTED_COUNT} tasks in both files, "
        f"got .zed={len(zed)} languages={len(lang)}"
    )


def test_tasks_store_no_password_in_env():
    for path in (ZED_TASKS, LANG_TASKS):
        tasks = json.loads(_read(path).decode("utf-8"))
        for task in tasks:
            env = task.get("env", {})
            assert isinstance(env, dict), (
                f"{path.name} task {task.get('label')!r}: env must be an object"
            )
            assert "MIKROTIK_PASS" not in env, (
                f"{path.name} task {task.get('label')!r} must not store "
                "MIKROTIK_PASS in env (pass via process env/keychain only)"
            )
            lowered = {str(k).lower() for k in env}
            assert not {k for k in lowered if "password" in k or "secret" in k}, (
                f"{path.name} task {task.get('label')!r}: env must not carry "
                f"password/secret entries, got {sorted(env)}"
            )
