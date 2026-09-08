"""Shared helpers for the MikroTik companion scripts.

Single source for the connection-setup logic that ``mikrotik-deploy.py`` and
``mikrotik-live-check.py`` share: REST scheme resolution, integer env-var
parsing, host validation, and IPv6 bracket formatting for URLs.

Mirrors the Rust counterparts in ``lsp/src/live.rs`` (``validate_host``,
``format_host_for_url``, ``resolve_scheme``, and the ``parse_env_u16`` /
``parse_env_u64`` semantics): keep both sides in sync when the rules change.

Usage notes:
- ``mikrotik-deploy.py`` and ``mikrotik-live-check.py`` both run
  ``validate_host`` / ``format_host_for_url`` on their REST/SSH targets
  before any network access (lexical SSRF denylist, no DNS).
- ``resolve_scheme`` returns a ``(scheme, legacy_shim_fired)`` tuple for
  caller compatibility; the legacy ``--no-ssl-verify`` http fallback is
  removed (always ``False``) so ``MIKROTIK_SSL=0`` only disables
  verification and never changes the scheme. Plain HTTP requires explicit
  ``--http`` / ``MIKROTIK_HTTP=1``.
"""

from __future__ import annotations

import ipaddress
import os
import sys


def env_int(name: str, default: int) -> int:
    """Read an integer env var, falling back to ``default`` with a warning on bad input.

    Mirrors ``lsp/src/live.rs`` ``parse_env_u16`` / ``parse_env_u64``: the
    value is trimmed before parsing; a missing, empty, or unparseable value
    falls back to ``default`` (range checks are the caller's responsibility).
    """
    raw = os.getenv(name)
    if raw is None or not raw.strip():
        return default
    try:
        return int(raw.strip())
    except ValueError:
        print(f"warning: invalid {name}={raw!r}, using default {default}", file=sys.stderr)
        return default


def resolve_scheme(port: int, force_http: bool, no_ssl_verify: bool) -> tuple[str, bool]:
    """Resolve the REST URL scheme.

    Default is HTTPS on every port; plain HTTP requires an explicit opt-in
    via --http (or MIKROTIK_HTTP=1). SSL verification (--no-ssl-verify /
    MIKROTIK_SSL=0) only controls certificate validation, never the scheme.

    The legacy ``--no-ssl-verify`` http fallback on non-standard ports
    (anything outside 443/8729) is removed: it silently downgraded HTTPS to
    plain HTTP. Plain-HTTP-on-port-80 setups must pass ``--http``
    (or ``MIKROTIK_HTTP=1``) explicitly.

    Returns (scheme, legacy_shim_fired) where the flag is always False
    (kept for caller compatibility; the warning branch never fires).

    Intentional divergence from ``lsp/src/live.rs::resolve_scheme``, which
    still preserves the legacy fallback.
    """
    _ = (port, no_ssl_verify)
    return ("http" if force_http else "https"), False


def validate_host(host: str) -> str | None:
    """Validate a device host per ``lsp/src/live.rs::validate_host``.

    Returns None on success, an error string on failure.
    Checks: non-empty, <=253 chars, no null/control chars, no URI delimiters
    (@ ? # % space), no path separators (/ \\), plus a lexical SSRF denylist
    (no DNS): exact ``169.254.169.254``, ``metadata.google.internal``,
    ``metadata.google``, ``metadata.goog``, ``0.0.0.0``, ``::``
    (case-insensitive, bracket-tolerant) and whole ``169.254.0.0/16`` for
    IPv4 literals via stdlib ``ipaddress``.

    Intentional divergence from the Rust side: private/loopback ranges and
    IPv6 link-local stay ALLOWED here (routers live on LAN, and this path has
    no ALLOW_LOOPBACK-style escape hatch), while the cloud-metadata and IPv4
    link-local ranges — the exfiltration-relevant ones — are denied.
    """
    if not host:
        return "empty"
    if len(host) > 253:
        return "exceeds 253 chars"
    if "\0" in host:
        return "contains null byte"
    if any(ord(c) < 32 for c in host):
        return "contains control characters"
    # Rust also checks is_control (which covers \t \n etc), but we already cover <32
    # Also check for URI delimiters
    if "@" in host or "?" in host or "#" in host or "%" in host or " " in host:
        return "contains URI delimiter (@?#% or space)"
    if "/" in host or "\\" in host:
        return "host contains path separator"
    # Lexical SSRF denylist (no DNS): case-insensitive, bracket-tolerant.
    stripped = host.strip()
    lowered = stripped.lower()
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
    # Whole 169.254.0.0/16 for IPv4 literals (lexical, no DNS).
    try:
        addr = ipaddress.ip_address(inner)
    except ValueError:
        addr = None
    if addr is not None and addr.version == 4 and addr.is_link_local:
        return "SSRF denied host"
    return None


def format_host_for_url(host: str) -> str:
    """Wrap bare IPv6 literals with brackets for URL.

    Mirrors ``lsp/src/live.rs::format_host_for_url``.
    """
    if host.startswith("[") and host.endswith("]"):
        return host
    if ":" in host:
        return f"[{host}]"
    return host