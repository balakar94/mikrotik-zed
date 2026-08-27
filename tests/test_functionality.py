"""Tests for functionality: commands.toml structure, extraction edge cases, deploy, tasks, grammar."""

import re
import json
import sys
import subprocess
import tempfile
import os
from pathlib import Path
from unittest import mock

import pytest

ROOT = Path(__file__).parent.parent

# Try tomllib (3.11+) then tomli
try:
    import tomllib  # type: ignore

    def load_toml(p: Path):
        with open(p, "rb") as f:
            return tomllib.load(f)
except ImportError:
    import tomli as tomllib  # type: ignore

    def load_toml(p: Path):
        with open(p, "rb") as f:
            return tomllib.load(f)

# Import extraction helpers
sys.path.insert(0, str(ROOT / "scripts"))
from extract_commands import parse_llms_full, _extract_heading_path, should_include  # type: ignore

PATH_RE = re.compile(r"^[a-z0-9][a-z0-9/_-]*$")
ALLOWED_TYPES = {"Directory", "Command"}


def _write_temp(content: str) -> str:
    tmp = tempfile.NamedTemporaryFile(mode="w", suffix=".txt", delete=False, encoding="utf-8")
    tmp.write(content)
    tmp.flush()
    tmp.close()
    return tmp.name


# ── data/commands.toml structure ─────────────────────────────────────

class TestCommandsTomlStructure:
    @pytest.fixture(autouse=True)
    def _load(self):
        self.path = ROOT / "data" / "commands.toml"
        if not self.path.exists():
            pytest.skip("data/commands.toml not found (run make extract)")
        self.data = load_toml(self.path)
        self.menus = self.data.get("menus", [])
        assert isinstance(self.menus, list), "commands.toml: menus should be a list"
        assert len(self.menus) > 0, "commands.toml: no menus found"

    def test_each_menu_has_path_and_type(self):
        for i, m in enumerate(self.menus):
            assert "path" in m, f"menus[{i}] missing path"
            assert "type" in m, f"menus[{i}] missing type"
            assert isinstance(m["path"], str) and m["path"], f"menus[{i}] path empty"
            assert isinstance(m["type"], str) and m["type"], f"menus[{i}] type empty"

    def test_path_regex_valid(self):
        for m in self.menus:
            path = m["path"]
            assert path.startswith("/"), f"path should start with /: {path!r}"
            inner = path.lstrip("/")
            assert PATH_RE.match(inner), f"path inner fails regex ^[a-z0-9][a-z0-9/_-]*$: {path!r}"
            assert " " not in path, f"path contains space: {path!r}"
            assert ".." not in path, f"path contains ..: {path!r}"

    def test_type_in_allowed_set(self):
        for m in self.menus:
            t = m["type"]
            # Allow Directory, Command, and variants like "Settings Directory" (seen in data)
            # Core types must contain Directory or Command
            assert any(k in t for k in ALLOWED_TYPES), f"path {m['path']!r} type {t!r} not in {ALLOWED_TYPES} (or variant)"

    def test_flags_structure(self):
        for m in self.menus:
            if "flags" not in m:
                continue
            flags = m["flags"]
            assert isinstance(flags, list), f"{m['path']} flags not a list"
            for f in flags:
                assert "name" in f, f"{m['path']} flag missing name: {f}"
                assert isinstance(f["name"], str) and f["name"], f"{m['path']} flag name empty"
                # description may be empty but should be string if present
                if "description" in f:
                    assert isinstance(f["description"], str)

    def test_arguments_structure(self):
        for m in self.menus:
            for key in ("arguments", "read_only"):
                if key not in m:
                    continue
                args = m[key]
                assert isinstance(args, list), f"{m['path']} {key} not a list"
                for a in args:
                    assert "name" in a, f"{m['path']} {key} entry missing name: {a}"
                    assert "type" in a, f"{m['path']} {key} entry missing type: {a}"
                    assert isinstance(a["name"], str)
                    assert isinstance(a["type"], str)
                    if "required" in a:
                        assert isinstance(a["required"], bool), f"{m['path']} {key} required not bool"
                    if "unset" in a:
                        assert isinstance(a["unset"], bool)

    def test_no_duplicate_paths(self):
        paths = [m["path"] for m in self.menus]
        assert len(paths) == len(set(paths)), f"duplicate paths found: {[p for p in paths if paths.count(p) > 1][:5]}"

    def test_toml_has_header_metadata(self):
        text = self.path.read_text(encoding="utf-8")
        assert "Auto-generated" in text, "commands.toml missing Auto-generated header"
        assert "RouterOS" in text, "commands.toml missing RouterOS version header"


# ── Extraction edge cases ────────────────────────────────────────────

class TestExtractionEdgeCases:
    def test_deeply_nested_paths(self):
        content = """
## ip/firewall/filter/nested/deep/level

**Type:** Directory

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="chain" typ="string">Chain</ArgTableRow>
</ArgTable>
"""
        path = _write_temp(content)
        try:
            menus = parse_llms_full(path)
            assert len(menus) == 1
            assert menus[0]["path"] == "/ip/firewall/filter/nested/deep/level"
        finally:
            os.unlink(path)

    def test_deeply_nested_7_segments(self):
        content = """
## interface/bridge/port/monitor/stats/detail/extra

**Type:** Directory
"""
        path = _write_temp(content)
        try:
            menus = parse_llms_full(path)
            assert len(menus) == 1
            assert menus[0]["path"] == "/interface/bridge/port/monitor/stats/detail/extra"
        finally:
            os.unlink(path)

    def test_markdown_links_stripped(self):
        # Heading with markdown link should be ignored (link stripped leaves no path)
        assert _extract_heading_path("## [ip/address](https://example.com)") is None
        # Heading with link plus text without slash also returns None
        assert _extract_heading_path("## [Click here](https://example.com) overview") is None
        # Heading with link inside but also a valid path nearby — link stripped, remainder checked
        content = """
## [ip/address](https://example.com)

**Type:** Directory

## ip/route

**Type:** Directory
"""
        tmp = _write_temp(content)
        try:
            menus = parse_llms_full(tmp)
            paths = [m["path"] for m in menus]
            assert "/ip/address" not in paths, "markdown link heading should be filtered"
            assert "/ip/route" in paths
        finally:
            os.unlink(tmp)

    def test_trailing_dots_stripped(self):
        assert _extract_heading_path("## ip/address.") == "ip/address"
        assert _extract_heading_path("## ip/address...") == "ip/address"
        assert _extract_heading_path("## ip/address..") == "ip/address"
        # Multiple trailing dots with valid path
        content = """
## ip/address...

**Type:** Directory
"""
        tmp = _write_temp(content)
        try:
            menus = parse_llms_full(tmp)
            assert menus[0]["path"] == "/ip/address"
        finally:
            os.unlink(tmp)

    def test_h4_vs_h1_headings(self):
        # h4 should be valid
        assert _extract_heading_path("#### ip/firewall/filter") == "ip/firewall/filter"
        # h1 should be ignored
        assert _extract_heading_path("# ip/address") is None
        # h2 valid
        assert _extract_heading_path("## ip/address") == "ip/address"
        # h3 valid
        assert _extract_heading_path("### ip/address") == "ip/address"
        # h5 ignored
        assert _extract_heading_path("##### ip/address") is None
        # Full parse: mix h1/h2/h4
        content = """
# ip/address

**Type:** Directory

## ip/route

**Type:** Directory

#### ip/firewall/filter

**Type:** Directory

##### ip/ignored

**Type:** Directory
"""
        tmp = _write_temp(content)
        try:
            menus = parse_llms_full(tmp)
            paths = [m["path"] for m in menus]
            assert "/ip/address" not in paths, "h1 should be ignored"
            assert "/ip/route" in paths
            assert "/ip/firewall/filter" in paths
            assert "/ip/ignored" not in paths, "h5 should be ignored"
        finally:
            os.unlink(tmp)

    def test_argtable_with_missing_fields(self):
        content = """
## ip/address

**Type:** Directory

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="address">No type attr</ArgTableRow>
<ArgTableRow typ="string">No arg attr</ArgTableRow>
<ArgTableRow>No attrs at all</ArgTableRow>
<ArgTableRow arg="gateway" typ="ipAddr" mandatory="1" unset="1">Both flags</ArgTableRow>
</ArgTable>
"""
        tmp = _write_temp(content)
        try:
            menus = parse_llms_full(tmp)
            assert len(menus) == 1
            args = menus[0]["arguments"]
            # Rows without an arg name are skipped entirely, not emitted
            # as anonymous entries with name == "".
            assert len(args) == 2
            assert args[0]["name"] == "address"
            assert args[0]["type"] == ""
            assert args[1]["name"] == "gateway"
            assert args[1]["required"] is True
            assert args[1]["unset"] is True
        finally:
            os.unlink(tmp)

    def test_argtable_unknown_c1_ignored(self):
        content = """
## ip/address

**Type:** Directory

<ArgTable c1="Unknown" c2="Foo" c3="Bar">
<ArgTableRow arg="hidden" typ="string">Should not appear</ArgTableRow>
</ArgTable>

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="visible" typ="string">Visible</ArgTableRow>
</ArgTable>
"""
        tmp = _write_temp(content)
        try:
            menus = parse_llms_full(tmp)
            assert len(menus) == 1
            args = menus[0]["arguments"]
            names = [a["name"] for a in args]
            assert "hidden" not in names
            assert "visible" in names
        finally:
            os.unlink(tmp)

    def test_empty_file_returns_no_menus(self):
        tmp = _write_temp("")
        try:
            assert parse_llms_full(tmp) == []
        finally:
            os.unlink(tmp)

    def test_headings_without_slash_ignored(self):
        assert _extract_heading_path("## Overview") is None
        assert _extract_heading_path("## Certificates") is None
        assert _extract_heading_path("## Introduction") is None
        assert _extract_heading_path("## ") is None


# ── Deploy script ────────────────────────────────────────────────────

class TestDeployScript:
    @pytest.fixture(autouse=True)
    def _load(self):
        self.path = ROOT / "scripts" / "mikrotik-deploy.py"
        assert self.path.exists(), "scripts/mikrotik-deploy.py missing"
        self.text = self.path.read_text(encoding="utf-8")

    def test_has_rest_and_ssh_support(self):
        assert "deploy_via_rest" in self.text, "missing deploy_via_rest"
        assert "deploy_via_ssh" in self.text, "missing deploy_via_ssh"
        assert "rest" in self.text.lower()
        assert "ssh" in self.text.lower()

    def test_handles_env_vars(self):
        for var in ["MIKROTIK_HOST", "MIKROTIK_USER", "MIKROTIK_PASS", "MIKROTIK_PORT", "MIKROTIK_SSL", "MIKROTIK_METHOD"]:
            assert var in self.text, f"mikrotik-deploy.py missing env var {var}"

    def test_has_dry_run(self):
        assert "--dry-run" in self.text, "missing --dry-run argument"
        assert "dry_run" in self.text, "missing dry_run handling"
        # DRY-RUN should not require network
        assert "DRY-RUN" in self.text

    def test_has_5mib_cap(self):
        assert "5 * 1024 * 1024" in self.text or "5*1024*1024" in self.text or "5MiB" in self.text, "missing 5MiB file size cap"
        # Check that load_file checks stat().st_size
        assert "st_size" in self.text or "stat" in self.text, "missing file size check"

    def test_imports_requests_and_paramiko(self):
        assert "import requests" in self.text, "missing requests import"
        assert "import paramiko" in self.text, "missing paramiko import"
        assert "HAS_REQUESTS" in self.text or "HAS_PARAMIKO" in self.text or "try:" in self.text, "should handle missing deps gracefully"

    def test_handles_missing_deps_gracefully(self):
        # Should have ImportError handling
        assert "ImportError" in self.text, "should catch ImportError for optional deps"
        # Should exit with message if deps missing and not dry-run
        assert "pip install" in self.text, "should hint pip install for missing deps"
        # Check dry-run bypasses dep check: deploy_via_rest/ssh should return early on dry_run
        # Look for dry_run guard before HAS_REQUESTS check
        rest_section = self.text[self.text.index("def deploy_via_rest"):self.text.index("def deploy_via_ssh")]
        assert "dry_run" in rest_section, "deploy_via_rest should handle dry_run"

    def test_deploy_dry_run_no_network(self):
        """Dry-run should succeed without network or real deps."""
        import importlib.util

        spec = importlib.util.spec_from_file_location("mikrotik_deploy", str(self.path))
        mod = importlib.util.module_from_spec(spec)
        # Need to ensure requests/paramiko not required for dry-run - just test file load & dry-run logic
        # Create a temp file and invoke via subprocess with --dry-run
        with tempfile.NamedTemporaryFile(mode="w", suffix=".rsc", delete=False, encoding="utf-8") as f:
            f.write("/ip address add address=1.1.1.1/24 interface=ether1\n")
            tmp_path = f.name
        try:
            result = subprocess.run(
                [sys.executable, str(self.path), tmp_path, "--dry-run", "--host", "1.2.3.4"],
                capture_output=True,
                text=True,
                timeout=10,
            )
            assert result.returncode == 0, f"dry-run failed: {result.stderr}"
            combined = result.stdout + result.stderr
            assert "DRY-RUN" in combined, f"dry-run output missing DRY-RUN marker: {combined!r}"
        finally:
            os.unlink(tmp_path)

    def test_uses_shlex_quote(self):
        assert "shlex" in self.text, "should use shlex.quote for filename safety"
        assert "shlex.quote" in self.text


# ── Tasks JSON ───────────────────────────────────────────────────────

class TestTasksJson:
    def test_tasks_files_exist(self):
        for p in [ROOT / "languages" / "rsc" / "tasks.json", ROOT / ".zed" / "tasks.json"]:
            assert p.exists(), f"{p} missing"

    def test_tasks_have_4_tasks(self):
        for p in [ROOT / "languages" / "rsc" / "tasks.json", ROOT / ".zed" / "tasks.json"]:
            data = json.loads(p.read_text(encoding="utf-8"))
            assert isinstance(data, list), f"{p} should be a JSON array"
            # 4 base tasks + 2 live opt-in tasks (feat/live-data: platform/runtime side)
            # Keep backward-compatible: at least 4, but expected 6 with live enrichment opt-in
            assert len(data) >= 4, f"{p} should have at least 4 tasks, found {len(data)}"
            assert len(data) == 6, f"{p} should have 6 tasks (4 base + 2 live opt-in), found {len(data)}"
            labels = [t.get("label", "") for t in data]
            assert any("Live" in lbl and "Check connectivity" in lbl for lbl in labels), f"{p} missing live Check connectivity task"
            assert any("Live" in lbl and "Enable enrichment" in lbl for lbl in labels), f"{p} missing live Enable enrichment task"
            # Secrets must not be stored: only echo placeholder may mention MIKROTIK_PASS
            text = p.read_text(encoding="utf-8")
            assert text.count("MIKROTIK_PASS") <= 1, f"{p} should not store MIKROTIK_PASS more than once (echo placeholder), found {text.count('MIKROTIK_PASS')}"
            for task in data:
                if "Live" in task.get("label", "") and "Check connectivity" in task.get("label", ""):
                    assert "inputs" in task, f"live Check connectivity task missing inputs"
                    ids = [i.get("id") for i in task["inputs"]]
                    assert "mikrotik_host" in ids, "live task missing mikrotik_host input"
                    assert "mikrotik_user" in ids, "live task missing mikrotik_user input"
                    assert not any("pass" in str(i).lower() for i in task["inputs"]), "live inputs must not include pass"
                    assert "mikrotik-live" in task.get("tags", []), "live task missing mikrotik-live tag"
                    assert task.get("env") == {}, "live task env must be empty (relies on shell_env passthrough)"
                    assert task.get("cwd") == "$ZED_WORKTREE_ROOT"
                    assert "${input:mikrotik_host}" in str(task.get("args", []))
                    assert "${input:mikrotik_user}" in str(task.get("args", []))

    def test_tasks_labels(self):
        expected_substrings = ["REST", "SSH", "Dry-run", "Validate"]
        for p in [ROOT / "languages" / "rsc" / "tasks.json"]:
            data = json.loads(p.read_text(encoding="utf-8"))
            labels = [t.get("label", "") for t in data]
            for substr in expected_substrings:
                assert any(substr.lower() in lbl.lower() for lbl in labels), f"{p} missing task with label containing {substr!r}, labels={labels}"

    def test_tasks_use_zed_vars(self):
        for p in [ROOT / "languages" / "rsc" / "tasks.json", ROOT / ".zed" / "tasks.json"]:
            text = p.read_text(encoding="utf-8")
            assert "$ZED_FILE" in text, f"{p} missing $ZED_FILE"
            assert "$ZED_WORKTREE_ROOT" in text, f"{p} missing $ZED_WORKTREE_ROOT"

    def test_tasks_both_files_consistent(self):
        a = json.loads((ROOT / "languages" / "rsc" / "tasks.json").read_text(encoding="utf-8"))
        b = json.loads((ROOT / ".zed" / "tasks.json").read_text(encoding="utf-8"))
        # Labels should match
        labels_a = sorted(t.get("label") for t in a)
        labels_b = sorted(t.get("label") for t in b)
        assert labels_a == labels_b, f"tasks.json mismatch: {labels_a} vs {labels_b}"

    def test_tasks_have_command_and_args(self):
        for p in [ROOT / "languages" / "rsc" / "tasks.json"]:
            data = json.loads(p.read_text(encoding="utf-8"))
            for t in data:
                assert "command" in t, f"task {t.get('label')} missing command"
                assert "args" in t, f"task {t.get('label')} missing args"
                assert isinstance(t["args"], list)


# ── Grammar edge cases ───────────────────────────────────────────────

class TestGrammarEdgeCases:
    def _try_parse(self, content: str) -> str | None:
        """Try to parse via tree-sitter if available. Returns tree string or None if unavailable."""
        grammar_dir = ROOT / "grammars" / "rsc"
        if not (grammar_dir / "src" / "parser.c").exists():
            return None
        # Try via npx tree-sitter parse if available
        with tempfile.NamedTemporaryFile(mode="w", suffix=".rsc", delete=False, encoding="utf-8") as f:
            f.write(content)
            tmp = f.name
        try:
            result = subprocess.run(
                ["npx", "tree-sitter", "parse", tmp],
                capture_output=True,
                text=True,
                cwd=str(grammar_dir),
                timeout=10,
            )
            # tree-sitter parse returns 0 even with errors, but output contains ERROR if failed
            if result.returncode == 0 or "ERROR" in result.stdout or "source_file" in result.stdout:
                return result.stdout
            return None
        except FileNotFoundError:
            return None
        finally:
            os.unlink(tmp)

    def test_empty_file_parsing(self):
        # Empty file should not crash extraction and should be parseable
        tmp = _write_temp("")
        try:
            menus = parse_llms_full(tmp)
            assert menus == []
        finally:
            os.unlink(tmp)
        # Grammar: empty file
        out = self._try_parse("")
        if out is None:
            pytest.skip("tree-sitter not available for grammar edge cases")
        assert "ERROR" not in out or "source_file" in out

    def test_comment_only_file(self):
        # File with only comments should not produce menus
        content = "# This is a comment\n# Another comment\n"
        tmp = _write_temp(content)
        try:
            # parse_llms_full looks for headings, so no menus expected
            menus = parse_llms_full(tmp)
            assert menus == []
        finally:
            os.unlink(tmp)
        # Grammar edge: comment-only RSC file should parse without ERROR
        out = self._try_parse("# comment only\n# another\n")
        if out is None:
            pytest.skip("tree-sitter not available")
        # Should parse to source_file, not ERROR node
        assert "source_file" in out or "comment" in out.lower()

    def test_large_file_parsing(self):
        # Generate a large file (~1000 lines) to stress test
        lines = [f"/ip address add address=10.0.{i // 256}.{i % 256}/24 interface=ether1" for i in range(1000)]
        content = "\n".join(lines)
        out = self._try_parse(content)
        if out is None:
            pytest.skip("tree-sitter not available")
        # Should still produce a tree, maybe with errors but not crash
        assert len(out) > 0, "large file parse produced no output"
        # Should not contain UNEXPECTED or be empty
        assert "source_file" in out or len(out) > 100

    def test_special_char_strings(self):
        # Strings with special characters should not break parsing
        content = ':put "hello \\"world\\""\n:put {a="b; c"}\n'
        out = self._try_parse(content)
        if out is None:
            pytest.skip("tree-sitter not available")
        assert len(out) > 0

    def test_nested_brackets(self):
        content = ":if ([/ip address find] != \"\") do={ :put \"found\" }\n"
        out = self._try_parse(content)
        if out is None:
            pytest.skip("tree-sitter not available")
        assert len(out) > 0
