"""Tests for the WASM API usage gate (scripts/check_wasm_api_usage.py).

Why this exists: `std::process::id()` compiles for wasm32-wasip2 and then
panics at runtime inside Zed — it shipped in 0.7.0 and aborted
`language_server_command` on the first auto-download. No compiler lint catches
that class (`unimplemented!()`, not a missing symbol), so this local gate is
the regression guard. These tests pin its detection, its test-module skip, and
its reviewed opt-out.
"""

import importlib.util
from pathlib import Path

ROOT = Path(__file__).parent.parent
SCRIPT_PATH = ROOT / "scripts" / "check_wasm_api_usage.py"


def _load_script_module():
    """Load the gate script as a module (it must be side-effect free)."""
    assert SCRIPT_PATH.is_file(), f"missing {SCRIPT_PATH}"
    spec = importlib.util.spec_from_file_location("check_wasm_api_usage_under_test", SCRIPT_PATH)
    assert spec is not None and spec.loader is not None, f"cannot build import spec for {SCRIPT_PATH}"
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


gate = _load_script_module()


def _scan(tmp_path: Path, body: str):
    path = tmp_path / "fixture.rs"
    path.write_text(body, encoding="utf-8")
    return gate.scan_text(body, path)


def test_flags_process_id_in_production_code(tmp_path):
    violations = _scan(tmp_path, "fn f() -> u32 { std::process::id() }\n")
    assert [v.label for v in violations] == ["std::process::id"]
    assert violations[0].line == 1


def test_flags_the_other_banned_apis(tmp_path):
    body = (
        "use std::path::Path;\n"
        "fn a() { let _ = std::env::var(\"HOME\"); }\n"
        "fn b() { let _ = std::net::TcpStream::connect(\"x:1\"); }\n"
        "fn c() { let _ = std::thread::spawn(|| {}); }\n"
        "fn d() { let _ = std::process::Command::new(\"ls\"); }\n"
    )
    labels = sorted(v.label for v in _scan(tmp_path, body))
    assert labels == ["std::env::var", "std::net", "std::process::Command", "std::thread::spawn"]


def test_skips_cfg_test_modules(tmp_path):
    body = (
        "fn prod() -> u32 { std::process::id() }\n"
        "#[cfg(test)]\n"
        "mod tests {\n"
        "    fn temp() -> u32 { std::process::id() }\n"
        "}\n"
    )
    violations = _scan(tmp_path, body)
    assert [v.line for v in violations] == [1]


def test_skips_not_wasm_cfg_blocks(tmp_path):
    # `#[cfg(not(target_arch = "wasm32"))]` code is absent from the wasm build,
    # so it may use APIs that panic there (the platform::process_seed pattern).
    # The `"wasm32"` literal must survive string stripping for this to match.
    body = (
        "pub fn seed() -> u32 {\n"
        '    #[cfg(not(target_arch = "wasm32"))]\n'
        "    {\n"
        "        std::process::id()\n"
        "    }\n"
        '    #[cfg(target_arch = "wasm32")]\n'
        "    {\n"
        "        0\n"
        "    }\n"
        "}\n"
    )
    assert _scan(tmp_path, body) == []


def test_positive_wasm_cfg_is_still_scanned(tmp_path):
    # `#[cfg(target_arch = "wasm32")]` IS the wasm build: never skipped.
    body = (
        "pub fn seed() -> u32 {\n"
        '    #[cfg(target_arch = "wasm32")]\n'
        "    {\n"
        "        std::process::id()\n"
        "    }\n"
        "}\n"
    )
    assert [v.line for v in _scan(tmp_path, body)] == [4]


def test_test_module_braces_do_not_leak(tmp_path):
    # Nested braces and braces in strings inside the test module must not
    # unbalance the depth counter and re-expose production code to the scan.
    body = (
        "#[cfg(test)]\n"
        "mod tests {\n"
        "    fn t() {\n"
        "        if true {\n"
        "            let fmt = \"a{b}c\";\n"
        "            let _ = std::process::id();\n"
        "        }\n"
        "    }\n"
        "}\n"
        "fn after_visible() -> u32 { std::process::id() }\n"
    )
    violations = _scan(tmp_path, body)
    assert [v.line for v in violations] == [10]


def test_string_literal_and_comment_are_not_flagged(tmp_path):
    body = (
        "fn f() {\n"
        "    let _ = \"std::process::id\";\n"
        "    // std::process::id() would panic in the component\n"
        "}\n"
    )
    assert _scan(tmp_path, body) == []


def test_opt_out_marker_allows_a_reviewed_use(tmp_path):
    body = (
        "fn f() -> u32 { std::process::id() } // wasm-api-ok: reviewed, not reached on wasm\n"
    )
    assert _scan(tmp_path, body) == []


def test_opt_out_requires_a_reason(tmp_path):
    body = "fn f() -> u32 { std::process::id() } // wasm-api-ok:\n"
    assert [v.label for v in _scan(tmp_path, body)] == ["std::process::id"]


def test_run_returns_codes_for_clean_dirty_and_missing_trees(tmp_path):
    clean = tmp_path / "clean"
    clean.mkdir()
    (clean / "ok.rs").write_text("pub fn f() -> u32 { 1 }\n", encoding="utf-8")
    assert gate.run(["--src", str(clean)]) == 0

    dirty = tmp_path / "dirty"
    dirty.mkdir()
    (dirty / "bad.rs").write_text(
        "fn f() { let _ = std::env::var(\"X\"); }\n", encoding="utf-8"
    )
    assert gate.run(["--src", str(dirty)]) == 1

    assert gate.run(["--src", str(tmp_path / "absent")]) == 2


def test_real_shim_source_is_clean():
    # The shipped shim must pass the gate: production code keeps the pid in
    # platform::process_seed's native arm only, and test modules are skipped.
    violations = []
    for path in sorted((ROOT / "src").rglob("*.rs")):
        violations.extend(gate.scan_file(path))
    assert violations == [], "\n".join(v.render() for v in violations)
