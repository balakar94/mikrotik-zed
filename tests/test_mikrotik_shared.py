"""QA gate for scripts/_mikrotik_shared.py (STREAM D cohesion).

The shared module must be the single source for REST scheme resolution,
integer env-var parsing, host validation, and IPv6 bracket formatting used
by the two MikroTik companion scripts. No network: pure function checks
plus a CLI smoke test (--help / --dry-run) that also proves the scripts'
import bootstrap works when run as `python scripts/<name>.py`.
"""

import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).parent.parent
SCRIPTS = ROOT / "scripts"
SHARED_PY = SCRIPTS / "_mikrotik_shared.py"
DEPLOY_PY = SCRIPTS / "mikrotik-deploy.py"
LIVE_CHECK_PY = SCRIPTS / "mikrotik-live-check.py"

# The shared module lives next to the scripts (no package); make it importable.
sys.path.insert(0, str(SCRIPTS))

from _mikrotik_shared import (  # noqa: E402
    env_int,
    format_host_for_url,
    resolve_scheme,
    validate_host,
)


def _read(p: Path) -> str:
    return p.read_text(encoding="utf-8")


# ── resolve_scheme ────────────────────────────────────────────────

class TestResolveScheme:
    def test_https_default_on_standard_ports(self):
        assert resolve_scheme(443, False, False) == ("https", False)
        assert resolve_scheme(8729, False, True) == ("https", False)

    def test_nonstandard_port_with_verify_stays_https(self):
        # ssl_verify ON (no_ssl_verify=False): scheme is https on any port.
        assert resolve_scheme(80, False, False) == ("https", False)
        assert resolve_scheme(8080, False, False) == ("https", False)

    def test_legacy_shim_forces_http_with_flag(self):
        # ssl_verify OFF (no_ssl_verify=True) on a non-standard port
        # historically forced http.
        assert resolve_scheme(80, False, True) == ("http", True)
        assert resolve_scheme(8080, False, True) == ("http", True)
        # Standard ports never fire the shim, even with verify off.
        assert resolve_scheme(443, False, True) == ("https", False)
        assert resolve_scheme(8729, False, True) == ("https", False)

    def test_force_http_wins(self):
        assert resolve_scheme(443, True, True) == ("http", False)
        assert resolve_scheme(80, True, False) == ("http", False)


# ── env_int ───────────────────────────────────────────────────────

class TestEnvInt:
    def test_missing_returns_default(self, monkeypatch):
        monkeypatch.delenv("MIKROTIK_TIMEOUT", raising=False)
        assert env_int("MIKROTIK_TIMEOUT", 60) == 60

    def test_empty_returns_default(self, monkeypatch):
        monkeypatch.setenv("MIKROTIK_TIMEOUT", "   ")
        assert env_int("MIKROTIK_TIMEOUT", 60) == 60

    def test_valid_value(self, monkeypatch):
        monkeypatch.setenv("MIKROTIK_TIMEOUT", "42")
        assert env_int("MIKROTIK_TIMEOUT", 60) == 42

    def test_whitespace_padded_value_is_trimmed(self, monkeypatch):
        monkeypatch.setenv("MIKROTIK_TIMEOUT", "  42  ")
        assert env_int("MIKROTIK_TIMEOUT", 60) == 42

    def test_invalid_warns_and_falls_back(self, monkeypatch, capsys):
        monkeypatch.setenv("MIKROTIK_TIMEOUT", "bogus")
        assert env_int("MIKROTIK_TIMEOUT", 60) == 60
        assert "warning: invalid MIKROTIK_TIMEOUT" in capsys.readouterr().err


# ── validate_host ─────────────────────────────────────────────────

class TestValidateHost:
    def test_valid_hosts(self):
        assert validate_host("192.168.88.1") is None
        assert validate_host("router.local") is None
        assert validate_host("[::1]") is None
        assert validate_host("fe80::1") is None

    def test_empty_and_overlong(self):
        assert validate_host("") == "empty"
        assert validate_host("a" * 254) == "exceeds 253 chars"

    def test_null_and_control_chars(self):
        assert validate_host("h\0st") == "contains null byte"
        assert validate_host("h\nst") == "contains control characters"

    def test_uri_delimiters_rejected(self):
        for bad in ["a@b", "a?b", "a#b", "a%b", "a b"]:
            assert validate_host(bad) is not None, f"should reject {bad!r}"

    def test_path_separators_rejected(self):
        assert validate_host("a/b") == "host contains path separator"
        assert validate_host("a\\b") == "host contains path separator"


# ── format_host_for_url ───────────────────────────────────────────

class TestFormatHostForUrl:
    def test_ipv4_and_hostname_unchanged(self):
        assert format_host_for_url("192.168.88.1") == "192.168.88.1"
        assert format_host_for_url("router.local") == "router.local"

    def test_bare_ipv6_bracketed(self):
        assert format_host_for_url("fe80::1") == "[fe80::1]"

    def test_already_bracketed_unchanged(self):
        assert format_host_for_url("[fe80::1]") == "[fe80::1]"


# ── Cohesion: both scripts consume the shared module ──────────────

class TestDedupCohesion:
    def test_shared_module_exists(self):
        assert SHARED_PY.exists(), "scripts/_mikrotik_shared.py missing"

    def test_both_scripts_import_shared(self):
        assert "from _mikrotik_shared import" in _read(DEPLOY_PY)
        assert "from _mikrotik_shared import" in _read(LIVE_CHECK_PY)

    def test_scripts_bootstrap_sys_path(self):
        # The scripts/ dir must be added to sys.path before the local import
        # so the import works from any CWD and under importlib loading.
        for script in (DEPLOY_PY, LIVE_CHECK_PY):
            text = _read(script)
            path_pos = text.index("sys.path.insert(0")
            import_pos = text.index("from _mikrotik_shared import")
            assert path_pos < import_pos, (
                f"{script.name}: sys.path bootstrap must precede the import"
            )

    def test_no_local_duplicated_definitions(self):
        deploy = _read(DEPLOY_PY)
        check = _read(LIVE_CHECK_PY)
        assert "def resolve_scheme" not in deploy
        assert "def _env_int" not in deploy
        assert "def resolve_scheme" not in check
        assert "def _env_int" not in check
        assert "def validate_host" not in check
        assert "def format_host_for_url" not in check


# ── CLI smoke: import bootstrap works when run as a script ───────

class TestCliSmoke:
    def test_deploy_help(self):
        result = subprocess.run(
            [sys.executable, str(DEPLOY_PY), "--help"],
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert result.returncode == 0, result.stderr
        assert "Deploy .rsc file to MikroTik RouterOS" in result.stdout

    def test_live_check_dry_run(self):
        env = {**os.environ, "MIKROTIK_HOST": "192.168.88.1"}
        result = subprocess.run(
            [sys.executable, str(LIVE_CHECK_PY), "--dry-run", "--host", "192.168.88.1"],
            capture_output=True,
            text=True,
            timeout=30,
            env=env,
        )
        assert result.returncode == 0, result.stderr
        assert "DRY-RUN" in result.stdout
        assert "https://192.168.88.1:443/rest/interface" in result.stdout

    def test_live_check_dry_run_ipv6_bracketed(self):
        result = subprocess.run(
            [sys.executable, str(LIVE_CHECK_PY), "--dry-run", "--host", "fe80::1"],
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert result.returncode == 0, result.stderr
        assert "https://[fe80::1]:443/rest/interface" in result.stdout

    def test_live_check_missing_host_still_usage_error(self):
        # Exit code contract unchanged: 2 = usage error (missing host).
        env = {k: v for k, v in os.environ.items() if k != "MIKROTIK_HOST"}
        result = subprocess.run(
            [sys.executable, str(LIVE_CHECK_PY), "--dry-run"],
            capture_output=True,
            text=True,
            timeout=30,
            env=env,
        )
        assert result.returncode == 2, f"stdout={result.stdout!r} stderr={result.stderr!r}"