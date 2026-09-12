"""QA gate for scripts/_mikrotik_shared.py (dedup cohesion).

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
    check_target,
    clamp_int,
    env_int,
    extract_spki_der,
    format_host_for_url,
    parse_fingerprint,
    redact_secrets,
    resolve_and_check_host,
    resolve_scheme,
    spki_sha256,
    validate_host,
    validate_user,
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