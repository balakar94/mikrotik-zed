"""QA gate for scripts/_mikrotik_shared.py (dedup cohesion).

The shared module must be the single source for REST scheme resolution,
integer env-var parsing, host validation, and IPv6 bracket formatting used
by the two MikroTik companion scripts. No network: pure function checks
plus a CLI smoke test (--help / --dry-run) that also proves the scripts'
import bootstrap works when run as `python scripts/<name>.py`.
"""

import contextlib
import http.server
import importlib.util
import ipaddress
import os
import shutil
import socket
import ssl
import subprocess
import sys
import threading
from pathlib import Path

import pytest

ROOT = Path(__file__).parent.parent
SCRIPTS = ROOT / "scripts"
SHARED_PY = SCRIPTS / "_mikrotik_shared.py"
DEPLOY_PY = SCRIPTS / "mikrotik-deploy.py"
LIVE_CHECK_PY = SCRIPTS / "mikrotik-live-check.py"

# The shared module lives next to the scripts (no package); make it importable.
sys.path.insert(0, str(SCRIPTS))

from _mikrotik_shared import (  # noqa: E402
    build_pinned_requests_session,
    build_pinned_urllib_handlers,
    check_target,
    check_target_with_addrs,
    clamp_int,
    embedded_ipv4,
    env_int,
    extract_spki_der,
    format_host_for_url,
    is_ipv6_transition_prefix,
    is_normalized_loopback_or_private,
    is_normalized_ssrf_denied,
    open_pinned_socket,
    parse_fingerprint,
    redact_secrets,
    resolve_and_check_host,
    resolve_host_addrs,
    resolve_scheme,
    spki_sha256,
    validate_host,
    validate_user,
    verify_spki_pin_on_socket,
)


def _read(p: Path) -> str:
    return p.read_text(encoding="utf-8")


# ── resolve_scheme ────────────────────────────────────────────────

class TestResolveScheme:
    def test_https_default_on_standard_ports(self):
        assert resolve_scheme(443, False) == "https"
        assert resolve_scheme(8729, False) == "https"

    def test_nonstandard_port_stays_https(self):
        # Scheme never depends on the port — only on the explicit opt-in.
        assert resolve_scheme(80, False) == "https"
        assert resolve_scheme(8080, False) == "https"

    def test_no_tls_downgrade_without_explicit_http(self):
        # Disabling verification must never change the scheme (the verify
        # flag is not even a parameter anymore); plain HTTP needs --http.
        assert resolve_scheme(80, False) == "https"
        assert resolve_scheme(8080, False) == "https"
        assert resolve_scheme(443, False) == "https"
        assert resolve_scheme(8729, False) == "https"

    def test_force_http_wins(self):
        assert resolve_scheme(443, True) == "http"
        assert resolve_scheme(80, True) == "http"


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
        # Link-local is fail-closed (SSRF deny fe80::/10)
        assert validate_host("fe80::1") is not None

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


class TestValidateHostSsrfDenylist:
    def test_exact_denials(self):
        for bad in [
            "169.254.169.254",
            "[169.254.169.254]",
            "metadata.google.internal",
            "metadata.google",
            "metadata.goog",
            "0.0.0.0",
            "::",
            "[::]",
            "[0.0.0.0]",
        ]:
            assert validate_host(bad) is not None, f"should deny {bad!r}"

    def test_denials_case_insensitive_and_bracket_tolerant(self):
        assert validate_host("Metadata.Google.Internal") is not None
        assert validate_host("METADATA.GOOGLE") is not None
        assert validate_host("[Metadata.Goog]") is not None
        assert validate_host("[169.254.169.254]") is not None

    def test_trailing_dot_hostnames_denied(self):
        # A trailing `.` is the DNS root / FQDN form: resolvers treat
        # `169.254.169.254.` and `metadata.google.internal.` as the same host,
        # but exact-string denials would miss them. Mirrors Rust
        # `test_ssrf_trailing_dot_hostnames_denied`.
        for bad in [
            "169.254.169.254.",
            "metadata.google.internal.",
            "metadata.google.",
            "metadata.goog.",
            "0.0.0.0.",
            "[metadata.goog].",
        ]:
            assert validate_host(bad) is not None, f"should deny trailing-dot {bad!r}"
        # Ordinary FQDN consumers are unaffected.
        assert validate_host("router.local.") is None
        # Numeric literals with a trailing dot are non-canonical: fail-closed.
        assert validate_host("169.254.169.254.") == "SSRF denied host"

    def test_whole_link_local_range_denied(self):
        assert validate_host("169.254.0.1") is not None
        assert validate_host("169.254.10.20") is not None
        assert validate_host("169.254.255.255") is not None
        assert validate_host("[169.254.10.20]") is not None

    def test_adjacent_and_public_hosts_allowed(self):
        assert validate_host("169.253.1.1") is None
        assert validate_host("192.168.88.1") is None
        assert validate_host("router.local") is None

    def test_ipv4_special_use_ranges_denied(self):
        # Unconditional denials, mirroring Rust
        # is_normalized_ssrf_denied: multicast 224.0.0.0/4, reserved
        # 240.0.0.0/4, IETF protocol assignments 192.0.0.0/24, benchmarking
        # 198.18.0.0/15.
        for bad in [
            "224.0.0.1",  # multicast floor
            "239.255.255.255",  # multicast ceiling
            "240.0.0.0",  # reserved floor
            "255.255.255.255",  # broadcast
            "192.0.0.1",  # IETF protocol assignments
            "198.18.0.0",  # benchmarking floor
            "198.19.255.255",  # benchmarking ceiling
        ]:
            assert validate_host(bad) is not None, f"should deny {bad!r}"
            assert is_normalized_ssrf_denied(ipaddress.ip_address(bad)), bad

    def test_ipv4_special_use_adjacent_public_allowed(self):
        for good in [
            "223.255.255.255",  # below multicast
            "198.17.255.255",  # below benchmarking
            "198.20.0.0",  # above benchmarking
            "192.0.1.1",  # above 192.0.0.0/24
        ]:
            assert validate_host(good) is None, f"should allow {good!r}"
            assert not is_normalized_ssrf_denied(ipaddress.ip_address(good)), good

    def test_lexical_only_no_dns(self):
        # Hostnames that merely contain a denied string are not denied.
        assert validate_host("not169.254.169.254.example.com") is None


class TestSharedSsrfVectors:
    """Shared table with lsp/src/tests/live_ssrf.rs: every encoding of a
    denied address fails closed. Intentional divergence: loopback forms are
    allowed here by design (routers live on LAN; no ALLOW_LOOPBACK gate on
    this path), so only the unconditionally-denied subset is asserted."""

    def test_shared_vectors_denied(self):
        for bad in [
            "2130706433",  # decimal 127.0.0.1
            "0x7f000001",  # hex 127.0.0.1
            "0177.0.0.1",  # octal 127.0.0.1
            "127.1",  # short 127.0.0.1
            "169.254.0.0",
            "169.254.0.1",
            "169.254.255.254",
            "fe80::1",
            "FE80::abcd",
        ]:
            assert validate_host(bad) is not None, f"should deny {bad!r}"

    def test_mapped_loopback_allowed_by_design(self):
        # Intentional divergence from Rust: this path has no loopback gate,
        # so loopback forms that are not non-canonical stay allowed here.
        assert validate_host("127.0.0.1") is None
        assert validate_host("[::ffff:127.0.0.1]") is None


class TestIpv6TransitionAndPrivateRanges:
    """Item 2 parity: NAT64/Teredo/6to4 are unconditional denials; ULA and
    CGNAT are private (Rust gates them behind ALLOW_LOOPBACK; the scripts
    intentionally allow private LAN ranges, so they stay allowed here)."""

    def test_transition_prefixes_denied(self):
        for bad in [
            "64:ff9b::a9fe:a9fe",  # NAT64 embedding 169.254.169.254
            "64:ff9b::7f00:1",  # NAT64 embedding 127.0.0.1
            "2001::1",  # Teredo
            "2002:a9fe:a9fe::1",  # 6to4 embedding 169.254.169.254
        ]:
            assert validate_host(bad) is not None, f"should deny {bad!r}"
            assert resolve_and_check_host(bad, 443) is not None

    def test_transition_prefix_helper_and_embedded_ipv4(self):
        assert is_ipv6_transition_prefix(ipaddress.ip_address("64:ff9b::a9fe:a9fe"))
        assert is_ipv6_transition_prefix(ipaddress.ip_address("2001::1"))
        assert is_ipv6_transition_prefix(ipaddress.ip_address("2002:c0a8:0101::1"))
        assert not is_ipv6_transition_prefix(ipaddress.ip_address("2001:db8::1"))
        assert embedded_ipv4(ipaddress.ip_address("64:ff9b::a9fe:a9fe")) == ipaddress.ip_address(
            "169.254.169.254"
        )
        assert embedded_ipv4(ipaddress.ip_address("2002:c0a8:0101::1")) == ipaddress.ip_address(
            "192.168.1.1"
        )
        assert embedded_ipv4(ipaddress.ip_address("2001:db8::1")) is None

    def test_ula_and_cgnat_are_private_not_unconditional(self):
        # Parity with Rust is_normalized_loopback_or_private.
        for priv in [
            "fc00::1",
            "fd12:3456:789a::1",
            "100.64.0.1",
            "100.127.255.255",
            "10.0.0.1",
            "192.168.88.1",
            "127.0.0.1",
        ]:
            assert is_normalized_loopback_or_private(ipaddress.ip_address(priv)), priv
        for pub in ["8.8.8.8", "100.63.255.255", "100.128.0.0", "2001:db8::1", "2606:4700::1"]:
            assert not is_normalized_loopback_or_private(ipaddress.ip_address(pub)), pub
        # Scripts allow private LAN ranges by design (documented divergence).
        assert validate_host("[fd12:3456::1]") is None
        assert validate_host("100.64.0.1") is None


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
        for script in (DEPLOY_PY, LIVE_CHECK_PY):
            text = _read(script)
            assert "check_target" in text
            assert "clamp_int" in text
            assert "resolve_scheme" in text

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
        assert "def _clamp_deploy_timeout" not in deploy
        assert "def clamp_int" not in deploy
        assert "def check_target" not in deploy
        assert "def resolve_scheme" not in check
        assert "def _env_int" not in check
        assert "def clamp_int" not in check
        assert "def check_target" not in check
        assert "def validate_host" not in check
        assert "def format_host_for_url" not in check

    def test_no_dead_legacy_shim(self):
        # The (scheme, legacy_shim_fired) tuple is gone: no caller may
        # unpack or branch on a shim flag that was always False.
        for script in (DEPLOY_PY, LIVE_CHECK_PY):
            text = _read(script)
            assert "legacy_shim" not in text


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
        # Global IPv6 still dry-runs with bracketing …
        result = subprocess.run(
            [sys.executable, str(LIVE_CHECK_PY), "--dry-run", "--host", "2001:db8::1"],
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert result.returncode == 0, result.stderr
        assert "https://[2001:db8::1]:443/rest/interface" in result.stdout

    def test_live_check_dry_run_link_local_denied(self):
        # … while link-local is fail-closed even for dry-run (SSRF deny).
        result = subprocess.run(
            [sys.executable, str(LIVE_CHECK_PY), "--dry-run", "--host", "fe80::1"],
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert result.returncode != 0, result.stdout

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


# ── Deploy SSRF gate (must validate even on --dry-run, exit 2) ───

class TestDeploySsrfGate:
    def _deploy_dry_run(self, host: str, tmp_path=None):
        import tempfile

        if tmp_path is None:
            import tempfile as _tf

            with _tf.NamedTemporaryFile(mode="w", suffix=".rsc", delete=False, encoding="utf-8") as f:
                f.write("/ip address add address=1.1.1.1/24 interface=ether1\n")
                rsc = f.name
        else:
            rsc = str(tmp_path)
        try:
            env = {k: v for k, v in os.environ.items() if k != "MIKROTIK_HOST"}
            result = subprocess.run(
                [sys.executable, str(DEPLOY_PY), rsc, "--dry-run", "--host", host],
                capture_output=True,
                text=True,
                timeout=30,
                env=env,
            )
            return result
        finally:
            if tmp_path is None:
                try:
                    os.unlink(rsc)
                except OSError:
                    pass

    def test_deploy_denies_cloud_metadata_ip_on_dry_run(self):
        result = self._deploy_dry_run("169.254.169.254")
        assert result.returncode == 2, f"stdout={result.stdout!r} stderr={result.stderr!r}"
        assert "invalid host" in result.stderr.lower()

    def test_deploy_denies_metadata_google_on_dry_run(self):
        for bad in ("metadata.google", "metadata.google.internal"):
            result = self._deploy_dry_run(bad)
            assert result.returncode == 2, f"host={bad!r} stdout={result.stdout!r} stderr={result.stderr!r}"
            assert "invalid host" in result.stderr.lower()

    def test_deploy_denies_link_local_range_on_dry_run(self):
        result = self._deploy_dry_run("169.254.10.20")
        assert result.returncode == 2, f"stdout={result.stdout!r} stderr={result.stderr!r}"

    def test_deploy_valid_host_still_dry_runs(self):
        result = self._deploy_dry_run("192.168.88.1")
        assert result.returncode == 0, f"stdout={result.stdout!r} stderr={result.stderr!r}"
        assert "DRY-RUN" in (result.stdout + result.stderr)

    def test_deploy_uses_shared_validate_and_brackets(self):
        text = DEPLOY_PY.read_text(encoding="utf-8")
        assert "validate_host" in text
        assert "format_host_for_url" in text


# ── Deploy scheme + timeout separation ───────────────────────────

class TestDeploySchemeAndTimeout:
    def test_ssl_zero_alone_stays_https(self):
        import tempfile

        with tempfile.NamedTemporaryFile(mode="w", suffix=".rsc", delete=False, encoding="utf-8") as f:
            f.write("/ip address add address=1.1.1.1/24 interface=ether1\n")
            rsc = f.name
        try:
            env = {k: v for k, v in os.environ.items() if k not in ("MIKROTIK_HOST", "MIKROTIK_HTTP", "MIKROTIK_SSL")}
            env["MIKROTIK_SSL"] = "0"
            result = subprocess.run(
                [sys.executable, str(DEPLOY_PY), rsc, "--dry-run", "--host", "192.168.88.1", "--port", "80"],
                capture_output=True,
                text=True,
                timeout=30,
                env=env,
            )
            assert result.returncode == 0, f"stdout={result.stdout!r} stderr={result.stderr!r}"
            combined = result.stdout + result.stderr
            assert "https://192.168.88.1:80" in combined
            assert "http://192.168.88.1:80" not in combined
        finally:
            os.unlink(rsc)

    def test_clamp_helper_bounds_and_guards(self):
        # Deploy bounds 1..300 (was _clamp_deploy_timeout, now shared).
        assert clamp_int(0, 1, 300, 60) == 1
        assert clamp_int(-5, 1, 300, 60) == 1
        assert clamp_int(9999, 1, 300, 60) == 300
        assert clamp_int(60, 1, 300, 60) == 60
        assert clamp_int("42", 1, 300, 60) == 42
        # Non-numeric falls back to default with a warning, never raises.
        assert clamp_int("bogus", 1, 300, 60) == 60
        assert clamp_int(None, 1, 300, 60) == 60

    def test_live_check_bounds(self, capsys):
        # Live-check bounds 1..30 with default 5.
        assert clamp_int(0, 1, 30, 5) == 1
        assert clamp_int(31, 1, 30, 5) == 30
        assert clamp_int(5, 1, 30, 5) == 5
        assert clamp_int(None, 1, 30, 5) == 5
        assert clamp_int("bogus", 1, 30, 5) == 5
        assert "warning: invalid timeout 'bogus', using default 5" in capsys.readouterr().err


# ── Deploy REST body cap + redaction ─────────────────────────────

def _load_deploy_module():
    spec = importlib.util.spec_from_file_location("mikrotik_deploy", DEPLOY_PY)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


class _FakeStreamResponse:
    """Minimal requests.Response stand-in for the streamed-read helper."""

    def __init__(self, chunks, encoding="utf-8"):
        self._chunks = chunks
        self.encoding = encoding

    def iter_content(self, chunk_size=8192):
        for chunk in self._chunks:
            yield chunk


class TestDeployResponseCapAndRedaction:
    def test_capped_reader_returns_small_body(self):
        mod = _load_deploy_module()
        resp = _FakeStreamResponse([b"hello ", b"world"])
        assert mod._read_response_capped(resp, "pw", "admin") == "hello world"

    def test_capped_reader_rejects_oversize_body(self, capsys):
        mod = _load_deploy_module()
        # 65 * 8192 = 532480 > 512 KiB, delivered in fixed chunks.
        chunks = [b"x" * 8192 for _ in range(65)]
        with pytest.raises(SystemExit) as exc:
            mod._read_response_capped(_FakeStreamResponse(chunks), "s3cret", "admin")
        assert exc.value.code == 4
        err = capsys.readouterr().err
        assert "response too large" in err
        assert "s3cret" not in err

    def test_deploy_reads_all_bodies_with_stream_and_cap(self):
        text = DEPLOY_PY.read_text(encoding="utf-8")
        # Every session response is streamed and read through the helper.
        assert text.count("stream=True") >= 3
        assert text.count("_read_response_capped(") >= 4  # def + 3 call sites
        # Raw unbounded body accessors are gone.
        assert "resp.text" not in text
        assert "imp.text" not in text
        assert "put_resp.text" not in text

    def test_deploy_redacts_device_bodies(self):
        text = DEPLOY_PY.read_text(encoding="utf-8")
        # Success, fallback, and import paths all wrap device output.
        assert "print(redact_secrets(body, password, user))" in text
        assert "redact_secrets(imp_body[:1000], password, user)" in text
        assert "redact_secrets(out, password, user)" in text
        assert "redact_secrets(err, password, user)" in text
        # Body cap matches live-check / caps.rs (512 KiB).
        assert "MAX_RESPONSE_BYTES = 512 * 1024" in text


# ── validate_user ─────────────────────────────────────────────────

class TestValidateUser:
    def test_valid_names(self):
        assert validate_user("admin") == "admin"
        assert validate_user("  admin  ") == "admin"
        assert validate_user("user.name-01_x") == "user.name-01_x"

    def test_empty_and_overlong(self):
        assert validate_user("") is None
        assert validate_user("   ") is None
        assert validate_user("a" * 65) is None
        assert validate_user("a" * 64) == "a" * 64

    def test_control_chars_and_bad_charset(self):
        assert validate_user("ad\0min") is None
        assert validate_user("ad\nmin") is None
        assert validate_user("ad min") is None
        assert validate_user("ad@min") is None
        assert validate_user("admín") is None


# ── parse_fingerprint ─────────────────────────────────────────────

class TestParseFingerprint:
    HEX64 = "ab" * 32

    def test_valid_pin(self):
        pin = parse_fingerprint(f"sha256:{self.HEX64}")
        assert pin is not None and len(pin) == 32

    def test_prefix_case_insensitive_and_separators_stripped(self):
        pin = parse_fingerprint(f"SHA256:{self.HEX64[:32]}:{self.HEX64[32:]}")
        assert pin is not None and len(pin) == 32
        spaced = parse_fingerprint("sha256: " + " ".join(self.HEX64[i : i + 8] for i in range(0, 64, 8)))
        assert spaced is not None and spaced == pin

    def test_malformed_returns_none(self):
        assert parse_fingerprint(None) is None
        assert parse_fingerprint("") is None
        assert parse_fingerprint("   ") is None
        assert parse_fingerprint("sha256:abc") is None  # too short
        assert parse_fingerprint("sha256:" + "ab" * 33) is None  # too long
        assert parse_fingerprint("sha256:" + "zz" * 32) is None  # non-hex
        assert parse_fingerprint("md5:" + self.HEX64) is None  # wrong scheme


# ── redact_secrets ────────────────────────────────────────────────

class TestRedactSecrets:
    def test_password_and_basic_variants_redacted(self):
        import base64

        text = "login admin:secret failed secret"
        out = redact_secrets(text, "secret", "admin")
        assert "secret" not in out
        basic = base64.b64encode(b"admin:secret").decode("ascii")
        assert basic not in redact_secrets(f"header {basic}", "secret", "admin")
        lone = base64.b64encode(b"secret").decode("ascii")
        assert lone not in redact_secrets(f"token {lone}", "secret", "admin")
        assert "[REDACTED]" in out

    def test_no_password_leaves_text_untouched(self):
        assert redact_secrets("nothing secret here", None, "admin") == "nothing secret here"
        assert redact_secrets("", "secret", "admin") == ""


# ── resolve_and_check_host (numeric literals only: no DNS traffic) ─

class TestResolveAndCheckHost:
    def test_allowed_literals_pass(self):
        assert resolve_and_check_host("192.168.88.1", 443) is None
        assert resolve_and_check_host("127.0.0.1", 443) is None  # allowed by design
        assert resolve_and_check_host("2001:db8::1", 443) is None

    def test_denied_addresses_fail_closed(self):
        assert resolve_and_check_host("169.254.169.254", 443) is not None
        assert resolve_and_check_host("169.254.10.20", 443) is not None
        assert resolve_and_check_host("0.0.0.0", 443) is not None
        assert resolve_and_check_host("::", 443) is not None
        assert resolve_and_check_host("fe80::1", 443) is not None
        assert resolve_and_check_host("[::ffff:a9fe:a9fe]", 443) is not None
        assert resolve_and_check_host("224.0.0.1", 443) is not None
        assert resolve_and_check_host("198.18.0.1", 443) is not None

    def test_empty_host(self):
        assert resolve_and_check_host("", 443) == "empty host"


# ── check_target (lexical + DNS composition) ──────────────────────

class TestCheckTarget:
    def test_allowed_literal_passes_both_phases(self):
        assert check_target("192.168.88.1", 443) is None

    def test_lexical_denial_short_circuits(self):
        assert check_target("", 443) is not None
        assert check_target("169.254.169.254", 443) is not None
        assert check_target("127.1", 443) is not None  # non-canonical numeric

    def test_dns_phase_denial(self):
        assert check_target("0.0.0.0", 443) is not None
        assert check_target("fe80::1", 443) is not None


# ── DER / SPKI walker ─────────────────────────────────────────────

def _tlv(tag: int, value: bytes) -> bytes:
    assert len(value) < 128
    return bytes([tag, len(value)]) + value


def _synthetic_cert(spki: bytes, fields: int = 5) -> bytes:
    """Minimal DER shaped like a certificate for the walker.

    outer SEQ { tbs SEQ { [0] EXPLICIT, ``fields`` INTEGERs, spki }, sig }.
    """
    tbs_inner = _tlv(0xA0, _tlv(0x02, b"\x02"))
    tbs_inner += b"".join(_tlv(0x02, bytes([i])) for i in range(fields))
    tbs_inner += spki
    return _tlv(0x30, _tlv(0x30, tbs_inner) + _tlv(0x30, b"\x00"))


class TestDerSpkiWalker:
    SPKI = _tlv(0x30, _tlv(0x06, b"\x2a\x03") + _tlv(0x03, b"\x00\xde\xad\xbe\xef"))

    def test_extract_spki_from_well_formed_cert(self):
        import hashlib

        cert = _synthetic_cert(self.SPKI)
        assert extract_spki_der(cert) == self.SPKI
        assert spki_sha256(cert) == hashlib.sha256(self.SPKI).digest()

    def test_wrong_field_count_fails_closed(self):
        assert extract_spki_der(_synthetic_cert(self.SPKI, fields=4)) is None
        assert extract_spki_der(_synthetic_cert(self.SPKI, fields=6)) is None

    def test_malformed_input_fails_closed(self):
        assert extract_spki_der(b"") is None
        assert extract_spki_der(b"\x30") is None
        assert extract_spki_der(b"\x31\x03\x01\x02\x03") is None  # not a SEQUENCE
        cert = _synthetic_cert(self.SPKI)
        assert extract_spki_der(cert[:-3]) is None  # truncated
        assert spki_sha256(b"") is None
        assert spki_sha256(b"not-der") is None


# ── Rust parity tripwire ──────────────────────────────────────────

class TestRustParity:
    """Pin the shared SSRF constants on both sides of the language boundary.

    The Python helpers mirror ``lsp/src/live_net.rs`` by hand; if either
    side edits its denylist without the other, this fails. (Behavioral
    parity lives in the shared SSRF vector tables above + live_ssrf.rs.)
    """

    LITERALS = (
        "169.254.169.254",
        "metadata.google.internal",
        "metadata.google",
        "metadata.goog",
        "0.0.0.0",
        "169.254.0.0/16",
        "fe80::/10",
        "64:ff9b::/96",
        "2001::/32",
        "2002::/16",
        "fc00::/7",
        "100.64.0.0/10",
        "224.0.0.0/4",
        "240.0.0.0/4",
        "192.0.0.0/24",
        "198.18.0.0/15",
    )

    def test_denylist_literals_match_rust(self):
        rust = (ROOT / "lsp" / "src" / "live_net.rs").read_text(encoding="utf-8")
        assert "validate_host" in rust, "expected Rust validate_host to live in live_net.rs"
        for literal in self.LITERALS:
            assert literal in SHARED_PY.read_text(encoding="utf-8"), f"shared lost {literal!r}"
            assert literal in rust, f"Rust live_net.rs lost {literal!r}"


# ── Deploy SSH pre-credential resolve gate ──────────────────────────

class TestDeploySshResolveGate:
    def _run_ssh(self, host: str, dry_run: bool):
        import tempfile

        with tempfile.NamedTemporaryFile(mode="w", suffix=".rsc", delete=False, encoding="utf-8") as f:
            f.write("/ip address add address=1.1.1.1/24 interface=ether1\n")
            rsc = f.name
        try:
            env = {k: v for k, v in os.environ.items() if k != "MIKROTIK_HOST"}
            env["MIKROTIK_PASS"] = "dummy"
            argv = [sys.executable, str(DEPLOY_PY), rsc, "--method", "ssh", "--host", host]
            if dry_run:
                argv.append("--dry-run")
            return subprocess.run(argv, capture_output=True, text=True, timeout=30, env=env)
        finally:
            try:
                os.unlink(rsc)
            except OSError:
                pass

    def test_ssh_unresolvable_host_fails_closed_before_credentials(self):
        # `.invalid` never resolves (RFC 2606): lexical passes, the DNS
        # phase refuses with exit 4 before paramiko dials.
        result = self._run_ssh("nonexistent.invalid", dry_run=False)
        assert result.returncode == 4, f"stdout={result.stdout!r} stderr={result.stderr!r}"

    def test_ssh_dry_run_never_touches_network(self):
        # Same unresolvable host dry-runs cleanly: previews stay DNS-free.
        result = self._run_ssh("nonexistent.invalid", dry_run=True)
        assert result.returncode == 0, f"stdout={result.stdout!r} stderr={result.stderr!r}"
        assert "DRY-RUN" in (result.stdout + result.stderr)

    def test_ssh_dry_run_denied_host_still_exit_2(self):
        result = self._run_ssh("169.254.169.254", dry_run=True)
        assert result.returncode == 2, f"stdout={result.stdout!r} stderr={result.stderr!r}"


# ── Pinned resolve-then-connect (Python TOCTOU closure) ───────────


class _QuietHTTPServer(http.server.ThreadingHTTPServer):
    """ThreadingHTTPServer that swallows client resets (pin-mismatch tests)."""

    def handle_error(self, request, client_address):
        pass


@contextlib.contextmanager
def _local_http_server():
    """Run a loopback HTTP server; yield ``(port, seen)``.

    ``seen`` accumulates ``(path, host_header)`` for each GET, so tests can
    prove the request reached the pinned server and kept the original Host.
    """
    seen: list[tuple[str, str]] = []

    class _Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            seen.append((self.path, self.headers.get("Host", "")))
            body = b"[]"
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    server = _QuietHTTPServer(("127.0.0.1", 0), _Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server.server_address[1], seen
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def _disable_dns(monkeypatch):
    """Make every DNS lookup fail, so any success proves no re-resolution."""

    def _boom(*args, **kwargs):
        raise socket.gaierror("DNS disabled by test")

    monkeypatch.setattr(socket, "getaddrinfo", _boom)


class TestPinnedResolution:
    def test_resolve_host_addrs_returns_validated_ips(self):
        err, addrs = resolve_host_addrs("127.0.0.1", 443)
        assert err is None
        assert addrs == ["127.0.0.1"]

    def test_resolve_host_addrs_fail_closed(self):
        err, addrs = resolve_host_addrs("169.254.169.254", 443)
        assert err is not None and addrs == []

    def test_check_target_with_addrs_denies_lexically(self):
        err, addrs = check_target_with_addrs("169.254.169.254", 443)
        assert err is not None and addrs == []

    def test_check_target_compat_wrapper_unchanged(self):
        assert check_target("169.254.169.254", 443) is not None
        assert check_target("127.0.0.1", 443) is None


class TestOpenPinnedSocket:
    def test_connects_to_validated_ip_without_dns(self, monkeypatch):
        with _local_http_server() as (port, _seen):
            _disable_dns(monkeypatch)
            sock = open_pinned_socket(["127.0.0.1"], port, 5)
            try:
                peer = sock.getpeername()
                assert peer[0] in ("127.0.0.1", "::ffff:127.0.0.1")
            finally:
                sock.close()

    def test_all_addresses_fail_raises(self):
        with pytest.raises(OSError):
            open_pinned_socket(["127.0.0.1"], 1, 1)


class TestPinnedRequestsSession:
    def test_request_dials_pinned_ip_despite_unresolvable_host(self, monkeypatch):
        pytest.importorskip("requests")
        with _local_http_server() as (port, seen):
            _disable_dns(monkeypatch)
            session = build_pinned_requests_session(
                "admin", "secret", ["127.0.0.1"], "", True
            )
            try:
                resp = session.get(
                    f"http://does-not-resolve.invalid:{port}/rest/interface",
                    timeout=5,
                    allow_redirects=False,
                )
            finally:
                session.close()
            assert resp.status_code == 200
            # Reached the pinned loopback server, with the original Host header.
            assert seen == [("/rest/interface", f"does-not-resolve.invalid:{port}")]

    def test_pinned_adapter_disables_env_proxies(self):
        pytest.importorskip("requests")
        session = build_pinned_requests_session(
            "admin", "secret", ["127.0.0.1"], "", True
        )
        try:
            assert session.trust_env is False
            assert session.proxies == {}
        finally:
            session.close()


class TestDeploySshPinnedSocket:
    def test_ssh_connect_passes_pinned_socket_and_original_hostname(self, monkeypatch):
        mod = _load_deploy_module()
        calls: dict = {}
        sentinel = object()

        class _FakeClient:
            def load_system_host_keys(self):
                pass

            def set_missing_host_key_policy(self, policy):
                calls["policy_set"] = True

            def connect(self, **kwargs):
                calls.update(kwargs)
                # Stop before SFTP/exec: SystemExit is BaseException, so the
                # method's `except Exception` does not swallow it.
                raise SystemExit(7)

        class _FakeParamiko:
            SSHClient = _FakeClient

            @staticmethod
            def AutoAddPolicy():
                return object()

        def _fake_open(addrs, port, timeout, *args, **kwargs):
            calls["pinned_args"] = (list(addrs), port, timeout)
            return sentinel

        monkeypatch.setattr(mod, "paramiko", _FakeParamiko)
        monkeypatch.setattr(mod, "HAS_PARAMIKO", True)
        monkeypatch.setattr(
            mod, "check_target_with_addrs", lambda host, port: (None, ["127.0.0.1"])
        )
        monkeypatch.setattr(mod, "open_pinned_socket", _fake_open)

        with pytest.raises(SystemExit) as exc:
            mod.deploy_via_ssh(
                "127.0.0.1", "admin", "pw", 2222, "/system identity print\n", "x.rsc", False, False
            )
        assert exc.value.code == 7
        assert calls["pinned_args"] == (["127.0.0.1"], 2222, 15)
        assert calls["hostname"] == "127.0.0.1"
        assert calls["sock"] is sentinel
        # No --accept-host-key: the default (reject unknown) policy stands.
        assert "policy_set" not in calls


# ── Same-connection SPKI pin verification ────────────────────────


class _FakeTLSSocket:
    """Minimal established-TLS-socket stand-in exposing getpeercert()."""

    def __init__(self, der: bytes):
        self._der = der

    def getpeercert(self, binary_form=False):
        return self._der if binary_form else {}


class TestVerifySpkiPinOnSocketUnit:
    def test_matching_pin_passes(self):
        cert = _synthetic_cert(TestDerSpkiWalker.SPKI)
        pin = spki_sha256(cert)
        assert pin is not None
        verify_spki_pin_on_socket(_FakeTLSSocket(cert), pin)  # must not raise

    def test_wrong_pin_raises(self):
        cert = _synthetic_cert(TestDerSpkiWalker.SPKI)
        with pytest.raises(ssl.SSLError):
            verify_spki_pin_on_socket(_FakeTLSSocket(cert), b"\x00" * 32)

    def test_missing_cert_raises(self):
        with pytest.raises(ssl.SSLError):
            verify_spki_pin_on_socket(_FakeTLSSocket(b""), b"\x00" * 32)
        with pytest.raises(ssl.SSLError):
            verify_spki_pin_on_socket(None, b"\x00" * 32)

    def test_none_pin_is_noop(self):
        # No pin configured: even a missing socket is fine.
        verify_spki_pin_on_socket(None, None)


@contextlib.contextmanager
def _local_https_server(tmp_path):
    """Self-signed loopback HTTPS server; yield ``(port, seen, cert, pin)``.

    ``seen`` accumulates ``(path, authorization)``. The cert is generated with
    the ``openssl`` CLI (no Python crypto dependency); tests skip when the
    binary is unavailable.
    """
    openssl = shutil.which("openssl")
    if openssl is None:
        pytest.skip("openssl not available to generate a self-signed test cert")
    cert = tmp_path / "cert.pem"
    key = tmp_path / "key.pem"
    # Config file (rather than -subj/-addext) so both OpenSSL and LibreSSL
    # accept it; the SAN lets the CA-file test keep hostname verification on.
    cfg = tmp_path / "openssl.cnf"
    cfg.write_text(
        "[req]\n"
        "distinguished_name = dn\n"
        "x509_extensions = v3\n"
        "prompt = no\n"
        "\n"
        "[dn]\n"
        "CN = localhost\n"
        "\n"
        "[v3]\n"
        "subjectAltName = DNS:localhost\n"
        "basicConstraints = CA:TRUE\n",
        encoding="utf-8",
    )
    subprocess.run(
        [
            openssl,
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-keyout",
            str(key),
            "-out",
            str(cert),
            "-days",
            "1",
            "-nodes",
            "-config",
            str(cfg),
        ],
        check=True,
        capture_output=True,
    )
    seen: list[tuple[str, str | None]] = []

    class _Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            seen.append((self.path, self.headers.get("Authorization")))
            body = b"[]"
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    server_ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    server_ctx.load_cert_chain(str(cert), str(key))
    server = _QuietHTTPServer(("127.0.0.1", 0), _Handler)
    server.socket = server_ctx.wrap_socket(server.socket, server_side=True)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    pin = spki_sha256(ssl.PEM_cert_to_DER_cert(cert.read_text(encoding="utf-8")))
    try:
        yield server.server_address[1], seen, cert, pin
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


class TestPinnedTlsSameConnection:
    """Integration proof that the pin is checked on the request connection.

    A request only reaches the server when the pin matches, so the
    ``seen`` log distinguishes "verified before send" from "sent then
    rejected". Coverage limitation: this exercises the HTTP client paths; it
    does not assert the exact wire ordering inside a third-party TLS stack.
    """

    def test_matching_pin_request_reaches_server(self, monkeypatch, tmp_path):
        pytest.importorskip("requests")
        with _local_https_server(tmp_path) as (port, seen, _cert, pin):
            _disable_dns(monkeypatch)
            session = build_pinned_requests_session(
                "admin", "secret", ["127.0.0.1"], "", True, pin, "https"
            )
            try:
                resp = session.get(
                    f"https://localhost:{port}/rest/interface",
                    timeout=5,
                    allow_redirects=False,
                )
            finally:
                session.close()
            assert resp.status_code == 200
            assert len(seen) == 1
            assert seen[0][0] == "/rest/interface"
            assert seen[0][1] and seen[0][1].startswith("Basic ")

    def test_mismatched_pin_sends_no_request(self, monkeypatch, tmp_path):
        requests = pytest.importorskip("requests")
        with _local_https_server(tmp_path) as (port, seen, _cert, _pin):
            _disable_dns(monkeypatch)
            session = build_pinned_requests_session(
                "admin", "secret", ["127.0.0.1"], "", True, b"\x00" * 32, "https"
            )
            try:
                with pytest.raises(requests.exceptions.RequestException):
                    session.get(
                        f"https://localhost:{port}/rest/interface",
                        timeout=5,
                        allow_redirects=False,
                    )
            finally:
                session.close()
            assert seen == []

    def test_ca_file_keeps_chain_and_requires_pin(self, monkeypatch, tmp_path):
        requests = pytest.importorskip("requests")
        with _local_https_server(tmp_path) as (port, seen, cert, pin):
            _disable_dns(monkeypatch)
            # Correct pin + custom CA chain + hostname match: succeeds.
            session = build_pinned_requests_session(
                "admin", "secret", ["127.0.0.1"], str(cert), True, pin, "https"
            )
            try:
                resp = session.get(
                    f"https://localhost:{port}/", timeout=5, allow_redirects=False
                )
            finally:
                session.close()
            assert resp.status_code == 200
            assert len(seen) == 1
            # Same CA file + wrong pin: chain validates, pin must still fail.
            seen.clear()
            session = build_pinned_requests_session(
                "admin", "secret", ["127.0.0.1"], str(cert), True, b"\x00" * 32, "https"
            )
            try:
                with pytest.raises(requests.exceptions.RequestException):
                    session.get(
                        f"https://localhost:{port}/", timeout=5, allow_redirects=False
                    )
            finally:
                session.close()
            assert seen == []

    def test_urllib_fallback_verifies_pin_on_same_connection(self, tmp_path):
        import urllib.error
        import urllib.request

        with _local_https_server(tmp_path) as (port, seen, _cert, pin):
            def _open(pin_value):
                ctx = ssl._create_unverified_context()
                handlers = build_pinned_urllib_handlers(["127.0.0.1"], ctx, pin_value)
                opener = urllib.request.build_opener(*handlers)
                req = urllib.request.Request(f"https://localhost:{port}/", method="GET")
                return opener.open(req, timeout=5)

            with _open(pin) as resp:
                assert resp.status == 200
            assert len(seen) == 1
            seen.clear()
            with pytest.raises((urllib.error.URLError, OSError)):
                _open(b"\x00" * 32)
            assert seen == []