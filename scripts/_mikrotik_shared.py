"""Shared helpers for the MikroTik companion scripts.

Single source for the connection-setup logic that ``mikrotik-deploy.py`` and
``mikrotik-live-check.py`` share: REST scheme resolution, integer env-var
parsing and clamping, host validation, IPv6 bracket formatting, username
validation, TLS fingerprint parsing, SPKI pin helpers, pre-credential target
checks, and password redaction.

Mirrors the Rust counterparts in ``lsp/src/live_net.rs`` (``validate_host``,
``format_host_for_url``, ``normalized_host_ip``,
``is_non_canonical_numeric_host``, ``resolve_and_validate_host``,
``extract_spki_der``, ``spki_sha256``) and ``lsp/src/live_config.rs``
(``validate_user``, ``parse_fingerprint``, ``resolve_scheme``, and the
``parse_env_u16`` / ``parse_env_u64`` semantics): keep both sides in sync
when the rules change (``tests/test_mikrotik_shared.py::TestRustParity``
pins the shared denylist constants on both sides).

Usage notes:
- ``mikrotik-deploy.py`` and ``mikrotik-live-check.py`` both run
  ``validate_host`` / ``format_host_for_url`` on their REST/SSH targets
  before any network access (lexical + normalized SSRF checks, no DNS).
- ``resolve_scheme`` returns the scheme as a plain string. Default is HTTPS
  on every port; plain HTTP requires an explicit opt-in via ``--http`` (or
  ``MIKROTIK_HTTP=1``). SSL verification (``--no-ssl-verify`` /
  ``MIKROTIK_SSL=0``) only controls certificate validation, never the
  scheme. The legacy ``--no-ssl-verify`` http fallback is removed (the
  Rust side keeps an opt-in ``RSC_LS_LEGACY_HTTP_SHIM=1`` fallback via
  ``resolve_scheme_with_legacy``; the scripts have no such fallback by
  design).
- ``check_target_with_addrs`` composes the lexical check (``validate_host``)
  with the DNS-time re-check (``resolve_host_addrs``) for the
  pre-credential phase and returns the validated IP strings. Callers dial
  exactly those addresses via :func:`open_pinned_socket` (SSH) or
  :func:`build_pinned_requests_adapter` / :func:`build_pinned_urllib_handlers`
  (HTTP/S), so no second DNS lookup can be rebound (resolve-then-connect
  TOCTOU closed). ``check_target`` is the error-string-only wrapper.
  Dry-run paths must keep calling only the lexical ``validate_host`` so
  previews never touch the network.
- ``clamp_int`` is the single timeout clamp: callers pass their own bounds
  (deploy 1..300, live-check 1..30).
- Host range checks run against the normalized IP (see
  ``normalized_host_ip``): decimal (``2130706433``), hex (``0x7f000001``),
  octal (``0177.0.0.1``), short (``127.1``), IPv4-mapped IPv6
  (``::ffff:127.0.0.1``), whole ``169.254.0.0/16``, and IPv6 ``fe80::/10``
  are all denied fail-closed. Non-canonical numeric literals are rejected
  even when the normalized address would otherwise be allowed.
- ``MIKROTIK_PASS`` never appears in logs: use ``redact_secrets`` on any
  error text that could echo credentials (including base64 ``user:pass``).
  Matching is by substring and over-redacts by design (fail-safe:
  redaction can only hide too much, never leak).
"""

from __future__ import annotations

import base64
import binascii
import hashlib
import ipaddress
import os
import socket
import ssl
import sys
import urllib.parse


def env_int(name: str, default: int) -> int:
    """Read an integer env var, falling back to ``default`` with a warning on bad input.

    Mirrors ``lsp/src/live_config.rs`` ``parse_env_u16`` / ``parse_env_u64``:
    the value is trimmed before parsing; a missing, empty, or unparseable
    value falls back to ``default`` (range checks are the caller's
    responsibility — see :func:`clamp_int`).
    """
    raw = os.getenv(name)
    if raw is None or not raw.strip():
        return default
    try:
        return int(raw.strip())
    except ValueError:
        print(f"warning: invalid {name}={raw!r}, using default {default}", file=sys.stderr)
        return default


def clamp_int(value: object, lo: int, hi: int, default: int, name: str = "timeout") -> int:
    """Parse ``value`` as int and clamp it into ``[lo, hi]`` (never raises).

    Single timeout clamp for both companions: deploy passes ``(…, 1, 300,
    …)``, live-check passes ``(…, 1, 30, …)``. Anything unparseable
    (including None) falls back to ``default`` with a stderr warning;
    out-of-range values clamp to the nearest bound.
    """
    try:
        parsed = int(value)  # type: ignore[arg-type]
    except (ValueError, TypeError):
        print(f"warning: invalid {name} {value!r}, using default {default}", file=sys.stderr)
        parsed = default
    return min(max(parsed, lo), hi)


def resolve_scheme(port: int, force_http: bool) -> str:
    """Resolve the REST URL scheme.

    Default is HTTPS on every port; plain HTTP requires an explicit opt-in
    via --http (or MIKROTIK_HTTP=1). SSL verification (--no-ssl-verify /
    MIKROTIK_SSL=0) only controls certificate validation, never the scheme.

    The legacy ``--no-ssl-verify`` http fallback on non-standard ports
    (anything outside 443/8729) is removed: it silently downgraded HTTPS to
    plain HTTP. Plain-HTTP-on-port-80 setups must pass ``--http``
    (or ``MIKROTIK_HTTP=1``) explicitly.

    ``port`` is accepted for symmetry with
    ``lsp/src/live_config.rs::resolve_scheme`` but does not affect the
    result: the scheme depends only on the explicit ``force_http`` opt-in.

    Intentional divergence from ``lsp/src/live_config.rs::resolve_scheme``,
    which keeps a third ``ssl_verify`` parameter and an opt-in
    ``RSC_LS_LEGACY_HTTP_SHIM=1`` fallback (``resolve_scheme_with_legacy``)
    with a WARN. The scripts have no legacy fallback by design.
    """
    _ = port
    return "http" if force_http else "https"


def validate_user(raw: str) -> str | None:
    """Validate a device username per ``lsp/src/live_config.rs::validate_user``.

    Allows 1..64 chars matching ``^[A-Za-z0-9._-]+$``. Returns the trimmed
    value on success, None on failure (callers fall back to ``admin``).
    """
    trimmed = (raw or "").strip()
    if not trimmed or len(trimmed) > 64:
        return None
    if "\0" in trimmed or any(ord(c) < 32 or ord(c) == 127 for c in trimmed):
        return None
    if not all(c.isascii() and (c.isalnum() or c in "._-") for c in trimmed):
        return None
    return trimmed


def _parse_numeric_part(part: str) -> int | None:
    """Parse one IPv4 numeric part (decimal, 0x hex, or 0-prefixed octal)."""
    if not part:
        return None
    if part[:2].lower() == "0x":
        digits = part[2:]
        if not digits or any(c not in "0123456789abcdefABCDEF" for c in digits):
            return None
        try:
            return int(digits, 16)
        except ValueError:
            return None
    if len(part) > 1 and part[0] == "0" and part.isdigit():
        # Octal: digits must be 0-7 (a "0" prefix with 8/9 is malformed).
        if any(c not in "01234567" for c in part):
            return None
        try:
            return int(part, 8)
        except ValueError:
            return None
    if not part.isdigit():
        return None
    try:
        return int(part, 10)
    except ValueError:
        return None


def parse_ipv4_numeric(literal: str) -> ipaddress.IPv4Address | None:
    """Parse canonical and non-canonical IPv4 numeric literals (inet_aton).

    Handles decimal (``2130706433``), hex (``0x7f000001``, ``0x7f.0x0.0x0.0x1``),
    octal (``0177.0.0.1``), and short forms (``127.1``, ``10.1``) with
    classic inet_aton semantics: 1 part = 32-bit value, 2 parts = a.24bits,
    3 parts = a.b.16bits, 4 parts = a.b.c.d. Returns None for hostnames,
    IPv6 literals, or malformed input (fail-closed by the caller).
    """
    s = (literal or "").strip()
    if not s or ":" in s:
        return None
    # Only numeric/dot/hex characters can be an IPv4 numeric literal.
    if any(c not in "0123456789abcdefABCDEFxX." for c in s):
        return None
    parts = s.split(".")
    if not 1 <= len(parts) <= 4 or any(p == "" for p in parts):
        return None
    try:
        nums = [_parse_numeric_part(p) for p in parts]
    except Exception:
        return None
    if any(n is None for n in nums):
        return None
    vals = [int(n) for n in nums]  # type: ignore[arg-type]
    try:
        if len(vals) == 1:
            v = vals[0]
            if not 0 <= v <= 0xFFFFFFFF:
                return None
            return ipaddress.IPv4Address(v)
        if len(vals) == 2:
            a, b = vals
            if not 0 <= a <= 0xFF or not 0 <= b <= 0xFFFFFF:
                return None
            return ipaddress.IPv4Address((a << 24) | b)
        if len(vals) == 3:
            a, b, c = vals
            if not 0 <= a <= 0xFF or not 0 <= b <= 0xFF or not 0 <= c <= 0xFFFF:
                return None
            return ipaddress.IPv4Address((a << 24) | (b << 16) | c)
        a, b, c, d = vals
        if any(not 0 <= v <= 0xFF for v in (a, b, c, d)):
            return None
        return ipaddress.IPv4Address(bytes([a, b, c, d]))
    except (ipaddress.AddressValueError, ValueError):
        return None


def normalized_host_ip(host: str) -> ipaddress.IPv4Address | ipaddress.IPv6Address | None:
    """Return the normalized IP for ``host`` when it is numeric, else None.

    Mirrors ``lsp/src/live_net.rs::normalized_host_ip``: the HTTP client connects
    to the normalized host, so range checks must run against this value, not
    the raw string. Handles canonical literals via stdlib ``ipaddress``,
    non-canonical IPv4 numerics via :func:`parse_ipv4_numeric`, and bare IPv6
    via :func:`format_host_for_url` + ``urllib.parse`` hostname extraction.
    Returns None for domain names or unparsable hosts (callers fall back to
    lexical hostname checks). Never performs DNS.
    """
    stripped = (host or "").strip()
    if not stripped:
        return None
    inner = stripped
    if inner.startswith("[") and inner.endswith("]") and len(inner) >= 2:
        inner = inner[1:-1]
    # Strip any zone id (defense in depth; '%' is rejected earlier anyway).
    inner = inner.split("%")[0]
    # A single trailing dot is the DNS root / FQDN form (`169.254.169.254.`).
    # Strip it so a numeric literal still normalizes to its IP; leaving it in
    # would misclassify a denied address as a domain and bypass the denylist.
    if len(inner) > 1 and inner.endswith("."):
        inner = inner[:-1]
    try:
        return ipaddress.ip_address(inner)
    except ValueError:
        pass
    numeric = parse_ipv4_numeric(inner)
    if numeric is not None:
        return numeric
    # URL-level normalization for bracketed/edge forms (no DNS).
    try:
        parsed = urllib.parse.urlsplit(f"http://{format_host_for_url(stripped)}/")
        hostname = parsed.hostname
        if hostname:
            try:
                return ipaddress.ip_address(hostname)
            except ValueError:
                numeric2 = parse_ipv4_numeric(hostname)
                if numeric2 is not None:
                    return numeric2
    except Exception:
        pass
    return None


def is_non_canonical_numeric_host(host: str) -> bool:
    """Whether ``host`` is a non-canonical numeric literal (fail-closed).

    Mirrors ``lsp/src/live_net.rs::is_non_canonical_numeric_host``: when
    normalization yields an IP whose canonical string differs from the raw
    literal (modulo brackets and ASCII case), the input used a decimal, hex,
    octal, short, or otherwise non-canonical encoding and is rejected — even
    when the normalized address itself would be allowed.
    """
    stripped = (host or "").strip()
    if not stripped:
        return False
    inner = stripped
    if inner.startswith("[") and inner.endswith("]") and len(inner) >= 2:
        inner = inner[1:-1]
    norm = normalized_host_ip(stripped)
    if norm is None:
        return False
    if isinstance(norm, ipaddress.IPv4Address):
        return inner != str(norm)
    # IPv6: compare case-insensitively against compressed form.
    return inner.lower() != norm.compressed.lower()


def _in_ipv6_net(addr: ipaddress.IPv6Address, net: str) -> bool:
    """Membership test that never raises (fail-closed False on bad input)."""
    try:
        return addr in ipaddress.IPv6Network(net)
    except Exception:
        return False


def is_ipv6_transition_prefix(addr: ipaddress.IPv6Address) -> bool:
    """Whether ``addr`` is a NAT64/Teredo/6to4 IPv6 transition prefix.

    Exact prefixes: NAT64 well-known ``64:ff9b::/96``, Teredo ``2001::/32``,
    and 6to4 ``2002::/16``. Mirrors
    ``lsp/src/live_net.rs::is_ipv6_transition_prefix``.
    """
    if not isinstance(addr, ipaddress.IPv6Address):
        return False
    return (
        _in_ipv6_net(addr, "64:ff9b::/96")
        or _in_ipv6_net(addr, "2001::/32")
        or _in_ipv6_net(addr, "2002::/16")
    )


def embedded_ipv4(addr: ipaddress.IPv6Address) -> ipaddress.IPv4Address | None:
    """Best-effort embedded IPv4 extraction from a transition prefix.

    - NAT64 well-known ``64:ff9b::/96``: last 32 bits.
    - 6to4 ``2002::/16``: bits 16..48.
    - Teredo ``2001::/32``: last 32 bits, bitwise-inverted (obfuscated).
    Returns None otherwise. Mirrors ``lsp/src/live_net.rs::embedded_ipv4``.
    """
    if not isinstance(addr, ipaddress.IPv6Address):
        return None
    if _in_ipv6_net(addr, "64:ff9b::/96"):
        return ipaddress.IPv4Address(int(addr) & 0xFFFFFFFF)
    if _in_ipv6_net(addr, "2002::/16"):
        return addr.sixtofour
    if _in_ipv6_net(addr, "2001::/32"):
        teredo = addr.teredo
        if teredo:
            return teredo[1]
    return None


def is_normalized_ssrf_denied(addr: ipaddress.IPv4Address | ipaddress.IPv6Address) -> bool:
    """Whether a normalized IP is unconditionally SSRF-denied.

    Covers whole ``169.254.0.0/16`` link-local (not just ``.169.254``),
    IPv6 ``fe80::/10`` link-local, unspecified addresses, and the IPv6
    transition prefixes that tunnel IPv4 regardless of any loopback opt-in:
    NAT64 well-known ``64:ff9b::/96``, Teredo ``2001::/32``, and 6to4
    ``2002::/16``. IPv4-mapped IPv6 (``::ffff:a.b.c.d``) is unmapped to
    IPv4 first so ``[::ffff:a9fe:a9fe]`` (metadata IP) is denied as
    link-local; where a transition prefix carries an extractable embedded
    IPv4, the IPv4 deny/private policy is re-run on it as well.
    """
    if isinstance(addr, ipaddress.IPv6Address):
        mapped = addr.ipv4_mapped
        if mapped is not None:
            return is_normalized_ssrf_denied(mapped)
        if addr.is_unspecified:
            return True
        # Best-effort: re-run the IPv4 deny/private checks on an embedded
        # address first, then deny the transition prefix itself
        # unconditionally. Both paths are exercised even though the prefix
        # denial alone would suffice.
        embedded = embedded_ipv4(addr)
        if embedded is not None and (
            is_normalized_ssrf_denied(embedded) or is_normalized_loopback_or_private(embedded)
        ):
            return True
        # NAT64 / Teredo / 6to4 are unconditional denials: they tunnel IPv4
        # (including link-local/metadata and private space).
        if is_ipv6_transition_prefix(addr):
            return True
        try:
            if addr in ipaddress.IPv6Network("fe80::/10"):
                return True
        except Exception:
            pass
        return bool(addr.is_link_local)
    # IPv4.
    if addr.is_unspecified:
        return True
    try:
        if addr in ipaddress.IPv4Network("169.254.0.0/16"):
            return True
    except Exception:
        pass
    return bool(addr.is_link_local)


def is_normalized_loopback_or_private(
    addr: ipaddress.IPv4Address | ipaddress.IPv6Address,
) -> bool:
    """Whether a normalized IP is loopback/RFC1918/CGNAT/ULA private.

    Mirrors ``lsp/src/live_net.rs::is_normalized_loopback_or_private``:
    loopback, RFC1918, CGNAT ``100.64.0.0/10``, and ULA ``fc00::/7`` are
    private (denied unless ``RSC_LS_LIVE_ALLOW_LOOPBACK=1`` on the Rust
    side). Intentional divergence: the companion scripts' :func:`validate_host`
    does NOT call this — routers legitimately live on the LAN and the scripts
    have no allow-loopback escape hatch — so these ranges stay ALLOWED here.
    Retained for Rust parity and tests.
    """
    if isinstance(addr, ipaddress.IPv6Address):
        mapped = addr.ipv4_mapped
        if mapped is not None:
            return is_normalized_loopback_or_private(mapped)
        if addr.is_loopback:
            return True
        return _in_ipv6_net(addr, "fc00::/7")
    if addr.is_loopback:
        return True
    try:
        if addr in ipaddress.IPv4Network("10.0.0.0/8"):
            return True
        if addr in ipaddress.IPv4Network("172.16.0.0/12"):
            return True
        if addr in ipaddress.IPv4Network("192.168.0.0/16"):
            return True
        if addr in ipaddress.IPv4Network("100.64.0.0/10"):
            return True
    except Exception:
        pass
    return False


def validate_host(host: str) -> str | None:
    """Validate a device host per ``lsp/src/live_net.rs::validate_host``.

    Returns None on success, an error string on failure.
    Checks: non-empty, <=253 chars, no null/control chars, no URI delimiters
    (@ ? # % space), no path separators (/ \\), plus SSRF denials with no
    DNS: exact ``169.254.169.254``, ``metadata.google.internal``,
    ``metadata.google``, ``metadata.goog``, ``0.0.0.0``, ``::``
    (case-insensitive, bracket-tolerant), whole ``169.254.0.0/16`` for IPv4
    literals, IPv6 ``fe80::/10`` link-local, the NAT64/Teredo/6to4 transition
    prefixes (``64:ff9b::/96``, ``2001::/32``, ``2002::/16``), IPv4-mapped
    IPv6 unmapping, and non-canonical numeric literals (decimal/hex/octal/
    short, including the FQDN-root trailing-dot form such as
    ``169.254.169.254.``) rejected fail-closed via normalization (see
    ``normalized_host_ip``).

    Intentional divergence from the Rust side: private/loopback ranges stay
    ALLOWED here (routers live on LAN, and this path has no
    ALLOW_LOOPBACK-style escape hatch), while the cloud-metadata, link-local,
    and obfuscated-numeric ranges — the exfiltration-relevant ones — are
    denied.
    """
    if not host:
        return "empty"
    if len(host) > 253:
        return "exceeds 253 chars"
    if "\0" in host:
        return "contains null byte"
    if any(ord(c) < 32 or ord(c) == 127 for c in host):
        return "contains control characters"
    if "@" in host or "?" in host or "#" in host or "%" in host or " " in host:
        return "contains URI delimiter (@?#% or space)"
    if "/" in host or "\\" in host:
        return "host contains path separator"
    # Lexical SSRF denylist (no DNS): case-insensitive, bracket-tolerant.
    # Trailing-dot (FQDN root) form: strip exactly one trailing dot BEFORE
    # bracket stripping so `[169.254.169.254].` is handled too, so
    # `169.254.169.254.` and `metadata.google.internal.` cannot evade the
    # exact-match denials. Mirrors lsp/src/live_net.rs::is_ssrf_denied_host.
    stripped = host.strip()
    lowered = stripped.lower()
    if lowered.endswith(".") and len(lowered) > 1:
        lowered = lowered[:-1]
    inner = lowered
    if inner.startswith("[") and inner.endswith("]") and len(inner) >= 2:
        inner = inner[1:-1]
    if inner in (
        "169.254.169.254",
        "metadata.google.internal",
        "metadata.google",
        "metadata.goog",
        "0.0.0.0",
        "::",
    ):
        return "SSRF denied host"
    # Normalize-then-check (fail-closed): non-canonical numerics first, then
    # range checks against the normalized IP with IPv4-mapped unmapping.
    if is_non_canonical_numeric_host(host):
        # Canonical loopback/private literals are allowed here, so only
        # report non-canonical when the normalized address is NOT an
        # otherwise-allowed plain literal... Simpler and safer: reject all
        # non-canonical numerics fail-closed (matches Rust).
        return "non-canonical numeric host"
    norm = normalized_host_ip(host)
    if norm is not None and is_normalized_ssrf_denied(norm):
        return "SSRF denied host"
    return None


def format_host_for_url(host: str) -> str:
    """Wrap bare IPv6 literals with brackets for URL.

    Mirrors ``lsp/src/live_net.rs::format_host_for_url``.
    """
    if host.startswith("[") and host.endswith("]"):
        return host
    if ":" in host:
        return f"[{host}]"
    return host


def parse_fingerprint(raw: str | None) -> bytes | None:
    """Parse ``MIKROTIK_FINGERPRINT=sha256:<hex>`` into 32 raw bytes.

    Accepts the ``sha256:`` prefix case-insensitively, strips embedded
    ``:``/whitespace separators, and requires exactly 64 hex chars (SPKI
    SHA256). Returns None when unset, empty, or malformed (callers warn and
    fail closed when a value was supplied but unparsable).
    """
    if raw is None:
        return None
    trimmed = raw.strip()
    if not trimmed:
        return None
    hex_part = trimmed
    if len(hex_part) >= 7 and hex_part[:7].lower() == "sha256:":
        hex_part = hex_part[7:]
    compact = "".join(c for c in hex_part if c != ":" and not c.isspace())
    if len(compact) != 64 or any(c not in "0123456789abcdefABCDEF" for c in compact):
        return None
    try:
        return binascii.unhexlify(compact)
    except (binascii.Error, ValueError):
        return None


def _der_read_tlv(data: bytes, pos: int) -> tuple[int, int, int, int] | None:
    """Read one DER TLV; returns (tag, header_len, value_len, value_start)."""
    if pos + 2 > len(data):
        return None
    tag = data[pos]
    first = data[pos + 1]
    if first & 0x80 == 0:
        length = first & 0x7F
        start = pos + 2
        if start + length > len(data):
            return None
        return (tag, 2, length, start)
    nbytes = first & 0x7F
    if nbytes == 0 or nbytes > 4 or pos + 2 + nbytes > len(data):
        return None
    length = 0
    for b in data[pos + 2 : pos + 2 + nbytes]:
        length = length * 256 + b
    start = pos + 2 + nbytes
    if start + length > len(data):
        return None
    return (tag, 2 + nbytes, length, start)


def extract_spki_der(cert_der: bytes) -> bytes | None:
    """Extract the DER of subjectPublicKeyInfo from a DER certificate.

    Minimal DER walker (fail-closed None on malformed input); hashes the
    full SPKI SEQUENCE TLV per RFC 7469.
    """
    tlv = _der_read_tlv(cert_der, 0)
    if tlv is None or tlv[0] != 0x30:
        return None
    tbs = _der_read_tlv(cert_der, tlv[3])
    if tbs is None or tbs[0] != 0x30:
        return None
    tbs_start, tbs_len = tbs[3], tbs[2]
    tbs_end = tbs_start + tbs_len
    if tbs_end > len(cert_der):
        return None
    pos = tbs_start
    if pos < tbs_end and cert_der[pos] == 0xA0:
        item = _der_read_tlv(cert_der, pos)
        if item is None:
            return None
        pos = item[3] + item[2]
    for _ in range(5):
        if pos >= tbs_end:
            return None
        item = _der_read_tlv(cert_der, pos)
        if item is None:
            return None
        pos = item[3] + item[2]
    if pos >= tbs_end or cert_der[pos] != 0x30:
        return None
    item = _der_read_tlv(cert_der, pos)
    if item is None:
        return None
    total = item[1] + item[2]
    if pos + total > len(cert_der):
        return None
    return cert_der[pos : pos + total]


def spki_sha256(cert_der: bytes) -> bytes | None:
    """SHA256 over the leaf SPKI DER (None on malformed input)."""
    spki = extract_spki_der(cert_der)
    if spki is None:
        return None
    return hashlib.sha256(spki).digest()


def resolve_host_addrs(host: str, port: int) -> tuple[str | None, list[str]]:
    """Resolve ``host`` and re-run the SSRF deny policy on every IP (F1).

    ``validate_host`` runs lexical + normalized-literal checks at startup,
    but a hostname can resolve to a denied address at connect time (DNS
    rebinding / split horizon). This re-resolves via ``getaddrinfo`` and
    denies the connection when ANY returned IP is unconditionally
    SSRF-denied (whole ``169.254.0.0/16``, IPv6 ``fe80::/10``,
    unspecified, IPv4-mapped equivalents). Private/loopback ranges stay
    ALLOWED (routers live on LAN, mirroring :func:`validate_host`).

    Returns ``(None, ip_strings)`` when every resolved IP passes, else
    ``(error, [])`` (fail-closed: resolution failure, empty results,
    unparseable IPs, and any denied IP all refuse the connection before
    credentials are sent). The returned IP strings are the exact addresses a
    pinned connection must dial; callers must NOT re-resolve them. This is
    the single resolution point that closes the Python resolve-then-connect
    TOCTOU: the address checked here is the address dialed by
    :func:`open_pinned_socket` / the pinned ``requests`` adapter.
    """
    bare = (host or "").strip()
    if bare.startswith("[") and bare.endswith("]") and len(bare) >= 2:
        bare = bare[1:-1]
    if not bare:
        return "empty host", []
    try:
        infos = socket.getaddrinfo(bare, port, type=socket.SOCK_STREAM)
    except socket.gaierror as e:
        return f"dns resolution failed: {e}", []
    except Exception as e:
        return f"dns resolution failed: {e}", []
    if not infos:
        return "dns resolution returned no addresses", []
    ips: list[str] = []
    for info in infos:
        try:
            ip_str = info[4][0]
        except (IndexError, TypeError):
            return "dns resolution returned malformed address", []
        try:
            addr = ipaddress.ip_address(ip_str)
        except ValueError:
            return f"unparseable resolved IP {ip_str!r}", []
        if is_normalized_ssrf_denied(addr):
            return f"resolved IP denied: {ip_str}", []
        if ip_str not in ips:
            ips.append(ip_str)
    return None, ips


def resolve_and_check_host(host: str, port: int) -> str | None:
    """Error string for a denied/failed resolution, else None.

    Thin compatibility wrapper over :func:`resolve_host_addrs`; callers that
    need the validated addresses must use :func:`resolve_host_addrs` /
    :func:`check_target_with_addrs` directly.
    """
    err, _ = resolve_host_addrs(host, port)
    return err


def check_target_with_addrs(host: str, port: int) -> tuple[str | None, list[str]]:
    """Lexical + DNS-time target check returning the validated IP strings.

    Composes :func:`validate_host` (lexical + normalized-literal SSRF
    checks, no DNS) with :func:`resolve_host_addrs` (DNS re-check). Returns
    ``(None, addrs)`` when the target passes, else the error and no
    addresses. The caller must dial exactly these addresses (e.g. via
    :func:`open_pinned_socket` or a pinned ``requests`` adapter) so the
    resolve-then-connect TOCTOU is closed.
    """
    err = validate_host(host)
    if err:
        return err, []
    return resolve_host_addrs(host, port)


def check_target(host: str, port: int) -> str | None:
    """Lexical + DNS-time target check for the pre-credential phase.

    Error-string compatibility wrapper over :func:`check_target_with_addrs`.
    Callers keep a lexical-only ``validate_host`` gate at startup (exit-code
    semantics differ per script); this runs at the pre-credential point
    where DNS must be re-checked. Dry-run paths must NOT call this —
    previews stay network-free.
    """
    err, _ = check_target_with_addrs(host, port)
    return err


def open_pinned_socket(
    addrs: list[str],
    port: int,
    timeout: float | None,
    source_address: str | None = None,
    socket_options=None,
) -> socket.socket:
    """Connect to the first reachable pre-validated ``addrs`` entry.

    Every entry is an IP literal produced by :func:`resolve_host_addrs`, so
    no DNS lookup happens here — the address validated at check time is the
    address dialed (the resolve-then-connect TOCTOU is closed). Addresses are
    tried in resolver order (preserving dual-stack preference); the first
    successful connect wins. Raises the last ``OSError`` when all fail.
    """
    last_err: OSError | None = None
    for raw in addrs:
        try:
            addr = ipaddress.ip_address(raw)
        except ValueError as e:
            last_err = OSError(f"invalid pinned address {raw!r}: {e}")
            continue
        family = socket.AF_INET6 if addr.version == 6 else socket.AF_INET
        sock: socket.socket | None = None
        try:
            sock = socket.socket(family, socket.SOCK_STREAM)
            for opt in socket_options or ():
                sock.setsockopt(*opt)
            if source_address:
                sock.bind((source_address, 0))
            sock.settimeout(timeout)
            sock.connect((raw, port))
            return sock
        except OSError as e:
            last_err = e
            if sock is not None:
                try:
                    sock.close()
                except OSError:
                    pass
    if last_err is not None:
        raise last_err
    raise OSError("no pinned addresses available")


def build_pinned_requests_adapter(addrs: list[str]):
    """Build a ``requests`` adapter that dials only the pre-validated IPs.

    ``urllib3``'s connection classes are subclassed so ``_new_conn()``
    returns a socket to a pinned address from :func:`resolve_host_addrs`
    (via :func:`open_pinned_socket`; no second DNS lookup). The pool still
    uses the original hostname, so the ``Host`` header and TLS SNI/name
    verification are unchanged (urllib3 wraps the pinned socket with
    ``server_hostname=self.host``). Imported lazily so ``_mikrotik_shared``
    keeps working without ``requests`` installed.
    """
    import requests.adapters
    from urllib3.connection import HTTPConnection, HTTPSConnection
    from urllib3.connectionpool import HTTPConnectionPool, HTTPSConnectionPool
    from urllib3.poolmanager import PoolManager

    pinned = tuple(addrs)

    class _PinnedConnectMixin:
        def _new_conn(self):
            return open_pinned_socket(
                list(pinned),
                self.port,
                self.timeout,
                source_address=getattr(self, "source_address", None),
                socket_options=getattr(self, "socket_options", None),
            )

    class _PinnedHTTPSConnection(_PinnedConnectMixin, HTTPSConnection):
        pass

    class _PinnedHTTPConnection(_PinnedConnectMixin, HTTPConnection):
        pass

    class _PinnedHTTPConnectionPool(HTTPConnectionPool):
        ConnectionCls = _PinnedHTTPConnection

    class _PinnedHTTPSConnectionPool(HTTPSConnectionPool):
        ConnectionCls = _PinnedHTTPSConnection

    class _PinnedPoolManager(PoolManager):
        def __init__(self, **kwargs):
            super().__init__(**kwargs)
            # Locally override the pool classes so every scheme uses the
            # pinned connection; set on the instance (urllib3 2.x).
            self.pool_classes_by_scheme = {
                "http": _PinnedHTTPConnectionPool,
                "https": _PinnedHTTPSConnectionPool,
            }

    class _PinnedHTTPAdapter(requests.adapters.HTTPAdapter):
        def init_poolmanager(self, connections, maxsize, block=False, **pool_kwargs):
            self.poolmanager = _PinnedPoolManager(
                num_pools=connections, maxsize=maxsize, block=block, **pool_kwargs
            )

    return _PinnedHTTPAdapter()


def build_pinned_requests_session(
    user: str,
    password: str,
    addrs: list[str],
    ca_file: str,
    ssl_verify: bool,
    pin: bytes | None = None,
    scheme: str = "https",
):
    """Build a ``requests.Session`` pinned to the pre-validated ``addrs``.

    Attaches HTTP Basic auth, disables environment proxies (a proxy would
    reroute the request around the pinned IP), and applies TLS verification:
    a custom CA bundle wins; otherwise ``ssl_verify``. When a SPKI ``pin`` is
    configured for an ``https`` scheme, chain verification is required on the
    pinned connection unless a custom CA file is supplied — the pin itself is
    validated on that same connection by the pinned connection class.
    """
    import requests

    session = requests.Session()
    # Never let env proxies/`.netrc` reroute a pinned connection.
    session.trust_env = False
    session.proxies = {}
    session.auth = (user, password)
    session.mount("http://", build_pinned_requests_adapter(addrs))
    session.mount("https://", build_pinned_requests_adapter(addrs))
    if pin is not None and scheme == "https":
        # Pin set: never TLS-verify against the system roots only.
        session.verify = ca_file if ca_file else True
    else:
        session.verify = ca_file if ca_file else ssl_verify
    session.headers.update({"Content-Type": "application/json"})
    return session


def build_pinned_urllib_handlers(addrs: list[str], context=None) -> list:
    """Build ``urllib.request`` handlers pinning HTTP/HTTPS to ``addrs``.

    Used by the requests-less fallback in ``mikrotik-live-check.py`` so that
    path closes the same resolve-then-connect TOCTOU instead of re-resolving
    the hostname. ``context`` is the SSL context for HTTPS (ignored for plain
    HTTP). The caller should also install ``ProxyHandler({})`` semantics —
    this helper returns one — so no environment proxy reroutes the dial.
    """
    import http.client
    import urllib.request

    pinned = tuple(addrs)

    class _PinnedHTTPConnection(http.client.HTTPConnection):
        def connect(self):
            self.sock = open_pinned_socket(list(pinned), self.port, self.timeout)
            if self._tunnel_host:
                self._tunnel()

    class _PinnedHTTPSConnection(http.client.HTTPSConnection):
        def connect(self):
            self.sock = open_pinned_socket(list(pinned), self.port, self.timeout)
            if self._tunnel_host:
                self._tunnel()
            self.sock = context.wrap_socket(self.sock, server_hostname=self.host)

    class _PinnedHTTPHandler(urllib.request.HTTPHandler):
        def http_open(self, req):
            return self.do_open(_PinnedHTTPConnection, req)

    class _PinnedHTTPSHandler(urllib.request.HTTPSHandler):
        def https_open(self, req):
            return self.do_open(_PinnedHTTPSConnection, req)

    return [
        _PinnedHTTPHandler(),
        _PinnedHTTPSHandler(),
        urllib.request.ProxyHandler({}),
    ]


def verify_tls_pin(
    host: str,
    port: int,
    pin: bytes,
    ca_file: str = "",
    timeout: int = 5,
    ssl_verify: bool = True,
) -> str | None:
    """Verify a device TLS SPKI pin over a fresh handshake (no HTTP).

    Opens a direct TLS connection to ``host:port``, extracts the leaf
    certificate, and compares ``SHA256(SPKI)`` against ``pin``. Chain
    validation follows ``ssl_verify``/``ca_file`` (custom CA bundle when
    provided). Returns None on success, an error string on failure
    (fail-closed: any network, validation, or pin mismatch is an error).
    Never performs HTTP or follows redirects.

    ``timeout`` must be a positive, already-clamped value (deploy 1..300,
    live-check 1..30 via :func:`clamp_int`); values below 1 are floored to
    1 defensively — a 0 timeout would flip the socket into non-blocking
    mode and fail in confusing ways.

    F1: DNS is re-resolved here and every returned IP is re-checked against
    the SSRF deny policy fail-closed before connecting (``host`` passed only
    lexical checks at startup; DNS may resolve differently now).
    """
    bare = host.strip()
    if bare.startswith("[") and bare.endswith("]") and len(bare) >= 2:
        bare = bare[1:-1]
    if timeout < 1:
        timeout = 1
    # F1: resolve-then-revalidate before any socket: a hostname that passed
    # lexical checks may still resolve to a denied address right now.
    dns_err = resolve_and_check_host(host, port)
    if dns_err:
        return dns_err
    try:
        if ssl_verify or ca_file.strip():
            ctx = ssl.create_default_context(cafile=(ca_file.strip() or None))
            if not ssl_verify:
                ctx.check_hostname = False
                ctx.verify_mode = ssl.CERT_NONE
        else:
            ctx = ssl._create_unverified_context()
            ctx.check_hostname = False
        raw_sock = socket.create_connection((bare, port), timeout=timeout)
    except Exception as e:
        return f"pin check connection failed: {e}"
    try:
        with ctx.wrap_socket(raw_sock, server_hostname=bare) as tls:
            try:
                der = tls.getpeercert(binary_form=True)
            except Exception as e:
                return f"pin check peer cert failed: {e}"
            if not der:
                return "pin check got no peer certificate"
            digest = spki_sha256(bytes(der))
            if digest is None:
                return "pin check could not parse peer SPKI"
            if digest != pin:
                return "TLS SPKI pin mismatch"
            return None
    except ssl.SSLCertVerificationError as e:
        return f"TLS certificate verification failed: {e}"
    except Exception as e:
        msg = str(e)
        if "pin mismatch" in msg.lower() or "certificate" in msg.lower():
            return msg
        return f"pin check TLS failed: {e}"
    finally:
        try:
            raw_sock.close()
        except Exception:
            pass


def redact_secrets(text: str, password: str | None, user: str | None = None) -> str:
    """Redact credential material from log/error text (central helper).

    Replaces the password, the base64 of ``user:password`` (HTTP Basic), and
    the base64 of the password alone. Never returns credential material;
    safe to apply unconditionally before printing or embedding in JSON.
    """
    out = text or ""
    secrets: list[str] = []
    if password:
        secrets.append(password)
        try:
            if user is not None:
                creds = f"{user}:{password}".encode("utf-8")
                secrets.append(base64.b64encode(creds).decode("ascii"))
        except Exception:
            pass
        try:
            secrets.append(base64.b64encode(password.encode("utf-8")).decode("ascii"))
        except Exception:
            pass
    for secret in secrets:
        if secret and secret in out:
            out = out.replace(secret, "[REDACTED]")
    return out
