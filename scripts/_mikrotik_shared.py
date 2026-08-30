"""Shared helpers for the MikroTik companion scripts.

Single source for the connection-setup logic that ``mikrotik-deploy.py`` and
``mikrotik-live-check.py`` share: REST scheme resolution, integer env-var
parsing, host validation, and IPv6 bracket formatting for URLs.

Mirrors the Rust counterparts in ``lsp/src/live.rs`` (``validate_host``,
``format_host_for_url``, ``resolve_scheme``, and the ``parse_env_u16`` /
``parse_env_u64`` semantics): keep both sides in sync when the rules change.

Usage notes:
- ``mikrotik-deploy.py`` imports ``resolve_scheme`` and ``env_int`` only; it
  deliberately does NOT run ``validate_host`` / ``format_host_for_url`` on
  its REST/SSH targets (its CLI behavior predates the extraction and is
  preserved as-is).
- ``resolve_scheme`` returns a ``(scheme, legacy_shim_fired)`` tuple so
  callers can warn about the legacy ``--no-ssl-verify`` http fallback; the
  live client applies the same rules without emitting that warning.
"""

from __future__ import annotations

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

    Legacy shim: --no-ssl-verify used to also force http:// on non-standard
    ports (anything outside 443/8729), which plain-HTTP-on-port-80 setups
    relied on. That observable behavior is preserved — with a warning — until
    those users migrate to --http.

    Returns (scheme, legacy_shim_fired).

    Mirrors ``lsp/src/live.rs::resolve_scheme``.
    """
    if not force_http and no_ssl_verify and port not in (443, 8729):
        return "http", True
    return ("http" if force_http else "https"), False


def validate_host(host: str) -> str | None:
    """Validate a device host per ``lsp/src/live.rs::validate_host``.

    Returns None on success, an error string on failure.
    Checks: non-empty, <=253 chars, no null/control chars, no URI delimiters
    (@ ? # % space), no path separators (/ \\).
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