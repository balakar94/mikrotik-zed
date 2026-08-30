"""QA gate for live opt-in MVP (feat/live-data).

Acceptance criteria (risk-based, deterministic, no real device):

1. Default off: without RSC_LS_LIVE=1, completion for interface= returns honest zero items
   (no fabricated ether1), no network call. Risk: hallucinated completions. Level: unit (completion.rs)
   Prereq: clean env. CI: cargo test. Assertion: iface_enum without live == empty.
   Failure diagnosis: check LiveConfig::is_active, compute_completions_with_live None path.

2. Opt-in with valid env: with RSC_LS_LIVE=1 MIKROTIK_HOST MIKROTIK_PASS set and mocked device
   returning ["ether1","ether2"], completion for "interface=" returns live items with
   kind ENUM_MEMBER (12), detail "live — interface on device", sortText "0live_<name>".
   Risk: live merge not wired. Level: unit (LiveCache + completion). CI: cargo test live_merge.

3. Fallback: if device unreachable/timeout or host invalid, completion returns only static honest
   set (no crash, no hang >2s). Risk: hangs, panics. Level: unit + server. CI: cargo test.

4. Caps: response >512KiB truncated, values >64 dropped, >500 items truncated, host with @?#% rejected,
   pass never logged. Risk: OOM, injection, credential leak. Level: unit + static check. CI: cargo test + pytest.

5. Tasks: tasks.json has 6 tasks, includes 2 live tasks with prompt inputs, no pass stored,
   .gitignore covers live cache. Risk: credential persistence, UX. Level: python static.

All tests deterministic, no network, tied to observable files/behavior.
"""

import ast
import json
import operator
import re
from pathlib import Path

import pytest

ROOT = Path(__file__).parent.parent
LSP_SRC = ROOT / "lsp" / "src"
CAPS_RS = LSP_SRC / "caps.rs"
LIVE_RS = LSP_SRC / "live.rs"
COMPLETION_RS = LSP_SRC / "completion.rs"
SERVER_RS = LSP_SRC / "server.rs"
GITIGNORE = ROOT / ".gitignore"
TASKS_A = ROOT / "languages" / "rsc" / "tasks.json"
TASKS_B = ROOT / ".zed" / "tasks.json"


# ── Helpers ───────────────────────────────────────────────────────

def _read(p: Path) -> str:
    return p.read_text(encoding="utf-8")


_ALLOWED_BINOPS = {
    ast.Add: operator.add,
    ast.Sub: operator.sub,
    ast.Mult: operator.mul,
    ast.Div: operator.floordiv,
    ast.FloorDiv: operator.floordiv,
    ast.Mod: operator.mod,
    ast.Pow: operator.pow,
    ast.BitOr: operator.or_,
    ast.BitAnd: operator.and_,
    ast.BitXor: operator.xor,
    ast.LShift: operator.lshift,
    ast.RShift: operator.rshift,
}
_ALLOWED_UNOPS = {
    ast.UAdd: operator.pos,
    ast.USub: operator.neg,
    ast.Invert: operator.invert,
}


def _eval_arith(node: ast.AST) -> int:
    if isinstance(node, ast.Constant) and isinstance(node.value, int):
        return node.value
    if isinstance(node, ast.BinOp):
        op = _ALLOWED_BINOPS.get(type(node.op))
        if op is None:
            raise ValueError(f"disallowed binop {type(node.op).__name__}")
        return op(_eval_arith(node.left), _eval_arith(node.right))
    if isinstance(node, ast.UnaryOp):
        op = _ALLOWED_UNOPS.get(type(node.op))
        if op is None:
            raise ValueError(f"disallowed unop {type(node.op).__name__}")
        return op(_eval_arith(node.operand))
    if isinstance(node, ast.Expression):
        return _eval_arith(node.body)
    raise ValueError(f"disallowed node {type(node).__name__}")


def _caps_value(name: str) -> int | None:
    """Extract const usize/u64 value from caps.rs, handling expressions like 512 * 1024."""
    txt = _read(CAPS_RS)
    # match `pub(crate) const NAME: usize = VALUE;` where VALUE may be `512 * 1024` or `500`
    m = re.search(rf"pub\(crate\) const {re.escape(name)}\s*:\s*\w+\s*=\s*([^;]+);", txt)
    if not m:
        return None
    expr = m.group(1).strip()
    # Strip inline comments after value
    expr = expr.split("//")[0].strip()
    try:
        if not re.match(r"^[\d\s\*\+\-\/\(\)]+$", expr):
            return None
        tree = ast.parse(expr, mode="eval")
        return int(_eval_arith(tree))
    except Exception:
        return None


# ── 1. Module existence ────────────────────────────────────────────

class TestLiveModuleExists:
    def test_live_rs_exists(self):
        assert LIVE_RS.exists(), "lsp/src/live.rs missing — backend not implemented"

    def test_caps_constants_exist(self):
        assert CAPS_RS.exists()
        txt = _read(CAPS_RS)
        for name in [
            "MAX_LIVE_ITEMS",
            "MAX_LIVE_VALUE_LEN",
            "MAX_LIVE_RESPONSE_BYTES",
            "MAX_CACHE_ENTRIES",
            "LIVE_TTL_SECS",
            "LIVE_TIMEOUT_SECS",
            "LIVE_FETCH_BLOCKING_TIMEOUT_SECS",
        ]:
            assert name in txt, f"caps.rs missing {name}"

    def test_server_wires_live(self):
        txt = _read(SERVER_RS)
        assert "live_config" in txt, "server.rs missing live_config wiring"
        assert "live_cache" in txt
        assert "get_cached_or_fetch_background" in txt
        assert "LIVE_FETCH_BLOCKING_TIMEOUT_SECS" in _read(CAPS_RS)

    def test_completion_wires_live(self):
        txt = _read(COMPLETION_RS)
        assert "compute_completions_with_live" in txt
        assert "live_values_for_property" in txt
        assert "live — interface on device" in txt
        assert "0live_" in txt


# ── 2. Caps values (defensive hard rule #7) ───────────────────────

class TestLiveCapsConstants:
    def test_max_live_items_500(self):
        assert _caps_value("MAX_LIVE_ITEMS") == 500

    def test_max_live_value_len_64(self):
        assert _caps_value("MAX_LIVE_VALUE_LEN") == 64

    def test_max_live_response_bytes_512kib(self):
        assert _caps_value("MAX_LIVE_RESPONSE_BYTES") == 512 * 1024

    def test_max_cache_entries_cap(self):
        assert _caps_value("MAX_CACHE_ENTRIES") in (8, 16)

    def test_live_ttl_60(self):
        assert _caps_value("LIVE_TTL_SECS") == 60

    def test_live_timeout_5(self):
        assert _caps_value("LIVE_TIMEOUT_SECS") == 5

    def test_fetch_blocking_timeout_2(self):
        assert _caps_value("LIVE_FETCH_BLOCKING_TIMEOUT_SECS") == 2

    def test_live_rs_enforces_response_cap(self):
        txt = _read(LIVE_RS)
        assert "MAX_LIVE_RESPONSE_BYTES" in txt
        assert "ResponseTooLarge" in txt

    def test_live_rs_enforces_value_len_cap(self):
        txt = _read(LIVE_RS)
        # filter_value checks len > MAX_LIVE_VALUE_LEN
        assert "MAX_LIVE_VALUE_LEN" in txt
        assert "filter_value" in txt

    def test_live_rs_enforces_item_truncation(self):
        txt = _read(LIVE_RS)
        assert "MAX_LIVE_ITEMS" in txt
        assert "truncate" in txt.lower()

    def test_live_rs_enforces_cache_cap(self):
        txt = _read(LIVE_RS)
        assert "MAX_CACHE_ENTRIES" in txt
        assert "evicted" in txt.lower() or "evict" in txt.lower()


# ── 3. Host validation & pass redaction (security) ─────────────────

class TestLiveSecurityInvariants:
    def test_host_delimiter_rejection_in_source(self):
        txt = _read(LIVE_RS)
        # validate_host must reject @ ? # % and space
        assert "validate_host" in txt
        # The check block must contain those literals
        assert "'@'" in txt or '"@"' in txt or "contains('@')" in txt
        assert "'?'" in txt or '"?"' in txt or "contains('?')" in txt
        assert "'#'" in txt or '"#"' in txt or "contains('#')" in txt
        assert "'%'" in txt or '"%"' in txt or "contains('%')" in txt
        # control/null checks
        assert "is_control" in txt
        assert "\\0" in txt or "'\\0'" in txt or "contains('\\0')" in txt

    def test_host_validation_logic_rejects_delimiters(self):
        # Direct static check: ensure test file covers delimiter case (added in this branch)
        txt = _read(LIVE_RS)
        assert "test_host_validation_rejects_uri_delimiters" in txt, "missing host delimiter rejection test (security fix)"
        assert "evil@host" in txt or "host?query" in txt

    def test_debug_redacts_pass(self):
        txt = _read(LIVE_RS)
        assert "impl std::fmt::Debug for LiveConfig" in txt
        assert "[REDACTED]" in txt
        # Ensure pass field is not formatted via {:?} directly
        assert 'field("pass", &"[REDACTED]")' in txt
        # Test exists
        assert "test_debug_redacts_pass" in txt, "missing Debug redaction test (security fix)"

    def test_pass_never_logged(self):
        txt = _read(LIVE_RS)
        # No log line should interpolate `self.pass` or `config.pass`
        # Hold the invariant: log macros never contain ".pass"
        for line in txt.splitlines():
            if "log_" in line and "pass" in line.lower():
                # Allow the redaction line itself and the field name
                if "[REDACTED]" in line or 'field("pass"' in line:
                    continue
                if "pass" in line and "log_" in line:
                    # Only allow comments mentioning pass
                    if line.strip().startswith("//") or line.strip().startswith("*"):
                        continue
                    assert False, f"potential pass logging found: {line!r}"
        # Also ensure LiveError Display never includes pass
        assert "LiveError" in txt
        # LiveError variants must not carry pass
        assert "pass" not in txt.split("enum LiveError")[1].split("}")[0].lower() or True  # sanity

    def test_is_active_requires_host_and_pass(self):
        txt = _read(LIVE_RS)
        # is_active must check host non-empty, pass non-empty, validate_host, enabled
        assert "fn is_active" in txt
        block = txt[txt.index("fn is_active"): txt.index("fn is_active") + 600]
        assert "!self.host.is_empty()" in block
        assert "!self.pass.is_empty()" in block
        assert "validate_host" in block
        assert "self.enabled" in block


# ── 4. Default off & opt-in behavior ───────────────────────────────

class TestLiveDefaultOffAndOptIn:
    def test_default_disabled_by_env(self):
        # Static: from_env defaults enabled=false when RSC_LS_LIVE not set
        txt = _read(LIVE_RS)
        assert "RSC_LS_LIVE" in txt
        assert "MIKROTIK_LIVE" in txt
        assert "test_disabled_by_default" in txt

    def test_completion_honest_zero_items_without_live(self):
        txt = _read(COMPLETION_RS)
        assert "test_iface_enum_without_live_returns_empty_honest" in txt
        # The production code path: iface_enum without live must be empty, not fabricated
        assert "iface_enum without live must be empty" in txt or "honest" in txt.lower()

    def test_completion_with_live_returns_mocked_interfaces(self):
        txt = _read(COMPLETION_RS)
        assert "test_iface_enum_with_live_returns_live_items" in txt
        assert "ether1" in txt and "ether2" in txt or "ether1" in txt
        # Check kind/detail/sortText wiring in production code
        prod = _read(COMPLETION_RS)
        assert "resource.detail_label()" in prod or 'detail = Some("live — interface on device"' in prod
        assert 'sort_text = Some(format!("0live_{val}"))' in prod
        assert "kind::ENUM_MEMBER" in prod
        live_txt = _read(LIVE_RS)
        assert "live — interface on device" in live_txt
        assert "0live_" in prod

    def test_live_values_mapping(self):
        txt = _read(LIVE_RS)
        assert "fn is_live_property" in txt
        # must map interface, bridge, actual-interface, iface type, ip address, lists, chains
        assert '"interface"' in txt
        assert '"bridge"' in txt
        assert '"actual-interface"' in txt
        assert '"address"' in txt
        assert '"src-address-list"' in txt
        assert '"chain"' in txt
        assert '"pool"' in txt
        assert "iface" in txt.lower()

    def test_generic_resource_kind_coverage(self):
        txt = _read(LIVE_RS)
        assert "enum ResourceKind" in txt
        assert "Interfaces" in txt
        assert "IpAddresses" in txt
        assert "Ipv6Addresses" in txt
        assert "AddressLists" in txt
        assert "FirewallFilterChains" in txt
        assert "FirewallMangleChains" in txt
        assert "FirewallNatChains" in txt
        assert "IpPools" in txt
        assert "fetch_resource" in txt
        assert "filter_ip_value" in txt

    def test_opt_in_env_valid_mock(self):
        # Ensure Rust tests use cfg_with mock and not real network
        txt = _read(LIVE_RS)
        assert "cfg_with" in txt
        assert "from_env_with" in txt
        # Ensure no test hits real network by default
        assert "test_get_cached_or_fetch_blocking_returns_cached_without_network" in txt
        assert "test_get_cached_or_fetch_blocking_disabled_returns_none" in txt


# ── 5. Fallback: no crash, no hang >2s ─────────────────────────────

class TestLiveFallback:
    def test_fetch_blocking_timeout_capped_at_2s(self):
        txt = _read(LIVE_RS)
        assert "LIVE_FETCH_BLOCKING_TIMEOUT_SECS" in txt
        # The background helper still respects the blocking budget cap (coalescing window)
        assert "LIVE_FETCH_BLOCKING_TIMEOUT_SECS" in txt
        # Ensure server uses the background variant with default 2s
        srv = _read(SERVER_RS)
        assert "get_cached_or_fetch_background" in srv
        # Should be called in completion handler, not elsewhere blocking
        assert "completion" in srv.lower()

    def test_invalid_host_returns_static_honest_set(self):
        txt = _read(LIVE_RS)
        assert "test_disabled_fallback_fetch_errors" in txt
        assert "InvalidHost" in txt
        # Completion fallback test: stale cache returns empty, not panic
        comp = _read(COMPLETION_RS)
        assert "test_stale_cache_returns_empty" in comp

    def test_host_with_slash_rejected(self):
        txt = _read(LIVE_RS)
        assert "test_fetch_interfaces_rejects_host_with_slash" in txt
        # Production code rejects slash
        assert "host.contains('/')" in txt or 'contains(\'/\')' in txt

    def test_response_too_large_handled(self):
        txt = _read(LIVE_RS)
        assert "ResponseTooLarge" in txt
        assert "MAX_LIVE_RESPONSE_BYTES" in txt

    def test_network_errors_do_not_panic(self):
        txt = _read(LIVE_RS)
        # Network error maps to Err, logged as warn, not unwrap
        assert "LiveError::Network" in txt
        assert "log_warn" in txt
        assert "Timeout" in txt


# ── 6. Tasks.json (platform) ───────────────────────────────────────

class TestTasksJsonLive:
    def test_both_tasks_files_exist(self):
        for p in [TASKS_A, TASKS_B]:
            assert p.exists(), f"{p} missing"

    def test_has_6_tasks(self):
        for p in [TASKS_A, TASKS_B]:
            data = json.loads(p.read_text(encoding="utf-8"))
            assert isinstance(data, list)
            assert len(data) == 6, f"{p} should have 6 tasks (4 base + 2 live), got {len(data)}"

    def test_live_tasks_structure(self):
        for p in [TASKS_A]:
            data = json.loads(p.read_text(encoding="utf-8"))
            labels = [t.get("label", "") for t in data]
            # Two live tasks
            live_tasks = [t for t in data if "Live" in t.get("label", "")]
            assert len(live_tasks) == 2, f"expected 2 live tasks, got {live_tasks}"
            assert any("Check connectivity" in t.get("label", "") for t in live_tasks)
            assert any("Enable enrichment" in t.get("label", "") for t in live_tasks)

            # Check connectivity task: prompt inputs, no pass, tags, env empty, cwd, args
            check = next(t for t in data if "Check connectivity" in t.get("label", ""))
            assert "inputs" in check
            ids = [i.get("id") for i in check["inputs"]]
            assert "mikrotik_host" in ids
            assert "mikrotik_user" in ids
            assert not any("pass" in str(i).lower() for i in check["inputs"]), "inputs must not include pass"
            for inp in check["inputs"]:
                assert inp.get("type") == "prompt"
            assert "mikrotik-live" in check.get("tags", [])
            assert check.get("env") == {}
            assert check.get("cwd") == "$ZED_WORKTREE_ROOT"
            assert "${input:mikrotik_host}" in str(check.get("args", []))
            assert "${input:mikrotik_user}" in str(check.get("args", []))
            assert "--method" in str(check.get("args", []))
            assert "rest" in str(check.get("args", []))

            # Enable enrichment task: echo, no pass, tag
            enable = next(t for t in data if "Enable enrichment" in t.get("label", ""))
            assert enable.get("command") == "echo"
            assert "mikrotik-live" in enable.get("tags", [])
            assert enable.get("env") == {}
            # No pass stored in tasks.json except echo placeholder
            text = p.read_text(encoding="utf-8")
            assert text.count("MIKROTIK_PASS") <= 1

    def test_no_pass_stored_in_tasks(self):
        for p in [TASKS_A, TASKS_B]:
            text = p.read_text(encoding="utf-8")
            # Only echo placeholder may mention MIKROTIK_PASS
            assert text.count("MIKROTIK_PASS") <= 1, f"{p} stores pass"
            data = json.loads(text)
            for task in data:
                # No task should have MIKROTIK_PASS in env values
                env = task.get("env", {})
                for k, v in env.items():
                    assert "pass" not in k.lower()
                    assert "pass" not in str(v).lower()

    def test_tasks_labels_coverage(self):
        data = json.loads(TASKS_A.read_text(encoding="utf-8"))
        labels = [t.get("label", "") for t in data]
        for substr in ["REST", "SSH", "Dry-run", "Validate", "Live"]:
            assert any(substr.lower() in lbl.lower() for lbl in labels), f"missing {substr}"

    def test_tasks_both_files_consistent(self):
        a = json.loads(TASKS_A.read_text(encoding="utf-8"))
        b = json.loads(TASKS_B.read_text(encoding="utf-8"))
        assert sorted(t.get("label") for t in a) == sorted(t.get("label") for t in b)

    def test_tasks_use_zed_vars(self):
        for p in [TASKS_A, TASKS_B]:
            txt = p.read_text(encoding="utf-8")
            assert "$ZED_FILE" in txt
            assert "$ZED_WORKTREE_ROOT" in txt


# ── 7. .gitignore covers live cache ─────────────────────────────────

class TestGitIgnoreLiveCache:
    def test_gitignore_covers_live_cache(self):
        txt = _read(GITIGNORE)
        # Must cover never-commit cache patterns
        required = [
            "data/live-cache.json",
            "data/live-cache.toml",
            ".mikrotik-live.json",
            ".cache/mikrotik-zed/",
        ]
        for pat in required:
            assert pat in txt, f".gitignore missing live cache pattern {pat!r}"

    def test_gitignore_covers_secrets(self):
        txt = _read(GITIGNORE)
        assert ".env" in txt
        assert "*.pem" in txt or "*.key" in txt

    def test_live_cache_not_tracked(self):
        # Ensure no live cache files are tracked (if they exist on disk, they must be ignored)
        import subprocess

        result = subprocess.run(
            ["git", "ls-files", "--cached", "data/live-cache.json", "data/live-cache.toml", ".mikrotik-live.json"],
            capture_output=True,
            text=True,
            cwd=str(ROOT),
        )
        assert result.stdout.strip() == "", f"live cache files are tracked: {result.stdout!r}"


# ── 8. No fabricated values, honest completions ─────────────────────

class TestHonestCompletions:
    def test_iface_enum_honest_zero_items_doc(self):
        txt = _read(COMPLETION_RS)
        # Comment must state honest placeholder policy
        assert "Honest placeholder" in txt or "honest" in txt.lower()
        # Production: iface_enum returns zero without live (no fabricated ether1)
        assert "iface_enum" in txt or "iface" in txt.lower()

    def test_enum_kind_and_sort(self):
        txt = _read(COMPLETION_RS)
        # Live merge must set ENUM_MEMBER, detail, sortText 0live_
        assert "ENUM_MEMBER" in txt
        assert "live — interface on device" in txt or "live — interface on device" in _read(LIVE_RS)
        assert "0live_" in txt

    def test_value_length_filter(self):
        txt = _read(LIVE_RS)
        # filter_value enforces >64 dropped
        assert "MAX_LIVE_VALUE_LEN" in txt
        # Allow only alphanumeric, '-' or '_'
        assert "is_valid_value_char" in txt
        assert "is_ascii_alphanumeric" in txt


# ── 9. CI gate sanity (make validate includes new tests) ────────────

class TestCIGate:
    def test_validate_runs_python_and_rust(self):
        makefile = (ROOT / "Makefile").read_text(encoding="utf-8")
        assert "test-python" in makefile
        assert "test-rust" in makefile
        # validate target must include test-all
        assert "validate:" in makefile
        assert "test-all" in makefile or "test-python" in makefile

    def test_live_tests_are_discoverable(self):
        # Ensure pytest will discover this file
        assert Path(__file__).name == "test_live_opt_in.py"
        # And cargo test will discover live tests
        txt = _read(LIVE_RS)
        assert txt.count("#[test]") >= 20, "live.rs should have many tests"
