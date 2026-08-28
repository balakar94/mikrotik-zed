#!/usr/bin/env python3
"""
MikroTik Live health check — verify REST connectivity for LSP enrichment.

Mirrors `lsp/src/live.rs` LiveConfig::from_env semantics so the same env vars
work for both the deploy companion and the language server.

Env vars (mirrored in scripts/mikrotik-deploy.py and lsp/src/live.rs):
  MIKROTIK_HOST    - device host/IP (required)
  MIKROTIK_USER    - username (default: admin)
  MIKROTIK_PASS    - password (required, never logged)
  MIKROTIK_PORT    - REST port (default: 443)
  MIKROTIK_SSL     - "0" to disable TLS verification (REST)
  MIKROTIK_HTTP    - "1" to force plain HTTP (default: https)
  MIKROTIK_TIMEOUT - per-request timeout seconds (1..30, default: 5 for live)

The check performs a real authenticated GET to /rest/interface and reports
item count. It never prints the password. Dry-run mode shows what would be
called without connecting.

Usage:
  python scripts/mikrotik-live-check.py --host 192.168.88.1 --user admin
  python scripts/mikrotik-live-check.py --dry-run
  MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret python scripts/mikrotik-live-check.py
  MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret python scripts/mikrotik-live-check.py --json

Exit codes:
  0 - Live OK (reachable, valid JSON)
  2 - Usage error (missing host)
  4 - Live FAIL (network, auth, status, parse, or host validation failure)
"""

from __future__ import annotations

import argparse
import getpass
import json
import os
import sys

# Optional requests - fallback to urllib
try:
    import requests  # type: ignore

    HAS_REQUESTS = True
except ImportError:
    requests = None  # type: ignore
    HAS_REQUESTS = False


def _env_int(name: str, default: int) -> int:
    """Read integer env var with warning on bad input (mirrors deploy companion)."""
    raw = os.getenv(name)
    if raw is None or not raw.strip():
        return default
    try:
        return int(raw.strip())
    except ValueError:
        print(f"warning: invalid {name}={raw!r}, using default {default}", file=sys.stderr)
        return default


def resolve_scheme(port: int, force_http: bool, no_ssl_verify: bool) -> tuple[str, bool]:
    """Resolve REST URL scheme — mirrors scripts/mikrotik-deploy.py::resolve_scheme.

    Default is HTTPS; plain HTTP requires explicit MIKROTIK_HTTP=1 / --http.
    Legacy shim: --no-ssl-verify on non-standard ports (outside 443/8729)
    historically forced http; preserved with warning semantics.

    Returns (scheme, legacy_shim_fired).
    """
    if not force_http and no_ssl_verify and port not in (443, 8729):
        return "http", True
    return ("http" if force_http else "https"), False


def validate_host(host: str) -> str | None:
    """Validate host per lsp/src/live.rs::validate_host.

    Returns None on success, error string on failure.
    Checks: non-empty, <=253, no null/control, no URI delimiters @?#% space, no slash.
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
    """Wrap bare IPv6 literals with brackets for URL."""
    if host.startswith("[") and host.endswith("]"):
        return host
    if ":" in host:
        return f"[{host}]"
    return host


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Check MikroTik Live REST connectivity")
    p.add_argument("--host", default=os.getenv("MIKROTIK_HOST"), help="Device host/IP (env MIKROTIK_HOST)")
    p.add_argument("--user", default=os.getenv("MIKROTIK_USER", "admin"), help="Username (env MIKROTIK_USER, default admin)")
    p.add_argument("--port", type=int, default=None, help="REST port (env MIKROTIK_PORT, default 443)")
    p.add_argument(
        "--no-ssl-verify",
        action="store_true",
        default=os.getenv("MIKROTIK_SSL") == "0",
        help="Disable TLS verification (env MIKROTIK_SSL=0)",
    )
    p.add_argument(
        "--http",
        action="store_true",
        default=os.getenv("MIKROTIK_HTTP") == "1",
        help="Force plain HTTP (env MIKROTIK_HTTP=1)",
    )
    p.add_argument(
        "--timeout",
        type=int,
        default=_env_int("MIKROTIK_TIMEOUT", 5),
        help="Request timeout seconds (env MIKROTIK_TIMEOUT, default 5, clamped 1..30)",
    )
    p.add_argument("--json", action="store_true", help="Output JSON instead of human text")
    p.add_argument("--dry-run", action="store_true", help="Show what would be called without connecting")
    # Compatibility shim for tasks.json that still passes --method rest (ignored, but required for test)
    p.add_argument("--method", choices=["rest", "auto", "ssh"], default="rest", help=argparse.SUPPRESS)
    return p.parse_args()


def main() -> None:
    args = parse_args()

    host = (args.host or "").strip()
    user = (args.user or "admin").strip()
    if not user:
        user = "admin"

    # Port resolution mirrors live.rs: default 443, env overrides
    port = args.port
    if port is None:
        port_raw = os.getenv("MIKROTIK_PORT")
        if port_raw is not None and port_raw.strip():
            try:
                port = int(port_raw.strip())
            except ValueError:
                print(f"warning: invalid MIKROTIK_PORT={port_raw!r}, using default 443", file=sys.stderr)
                port = 443
        else:
            port = 443

    # Clamp timeout 1..30 like live.rs
    timeout = args.timeout
    if timeout is None:
        timeout = 5
    try:
        timeout = int(timeout)
    except (ValueError, TypeError):
        print(f"warning: invalid timeout {timeout!r}, using default 5", file=sys.stderr)
        timeout = 5
    if timeout < 1:
        timeout = 1
    if timeout > 30:
        timeout = 30

    ssl_verify = not args.no_ssl_verify
    force_http = bool(args.http)

    # Host validation before any network
    if not host:
        print("error: --host or MIKROTIK_HOST is required", file=sys.stderr)
        print("hint: set MIKROTIK_HOST or pass --host 192.168.88.1", file=sys.stderr)
        if args.json:
            print(json.dumps({"ok": False, "error": "missing host", "host": host}))
        else:
            print("Live FAIL: missing host")
        sys.exit(2)

    err = validate_host(host)
    if err:
        msg = f"invalid host {host!r}: {err}"
        print(f"error: {msg}", file=sys.stderr)
        if args.json:
            print(json.dumps({"ok": False, "error": msg, "host": host}))
        else:
            print(f"Live FAIL: {msg}")
        sys.exit(4)

    if port == 0 or not (1 <= port <= 65535):
        msg = f"invalid port {port}"
        print(f"error: {msg}", file=sys.stderr)
        if args.json:
            print(json.dumps({"ok": False, "error": msg}))
        else:
            print(f"Live FAIL: {msg}")
        sys.exit(4)

    scheme, legacy_shim = resolve_scheme(port, force_http, not ssl_verify)
    if legacy_shim:
        print(
            "warning: --no-ssl-verify no longer selects the scheme; use --http (or MIKROTIK_HTTP=1) explicitly",
            file=sys.stderr,
        )

    host_for_url = format_host_for_url(host)
    url = f"{scheme}://{host_for_url}:{port}/rest/interface"

    # Dry-run: never require pass, never connect
    if args.dry_run:
        if args.json:
            print(
                json.dumps(
                    {
                        "dry_run": True,
                        "host": host,
                        "port": port,
                        "scheme": scheme,
                        "user": user,
                        "url": url,
                        "ssl_verify": ssl_verify,
                        "timeout": timeout,
                        "method": args.method,
                    }
                )
            )
        else:
            print(f"[mikrotik-live-check] DRY-RUN: would GET {url} as {user} (ssl_verify={ssl_verify}, timeout={timeout}s, method={args.method})")
            print(f"DRY-RUN: host={host} port={port} scheme={scheme} user={user} url={url}")
        sys.exit(0)

    # Password: env or prompt (never via argv, never logged)
    password = os.getenv("MIKROTIK_PASS")
    if not password:
        # Prompt securely if TTY available
        try:
            if sys.stdin.isatty():
                password = getpass.getpass(f"Password for {user}@{host}: ")
            else:
                password = None
        except Exception:
            password = None
        if not password:
            print("error: MIKROTIK_PASS is required (env or prompt)", file=sys.stderr)
            print("hint: export MIKROTIK_PASS=... or run with --dry-run to preview", file=sys.stderr)
            if args.json:
                print(json.dumps({"ok": False, "error": "missing MIKROTIK_PASS", "host": host, "url": url}))
            else:
                print("Live FAIL: missing MIKROTIK_PASS")
            sys.exit(2)

    # Never log password
    # Perform GET
    # Cap response at 512 KiB like caps.rs MAX_LIVE_RESPONSE_BYTES
    MAX_BYTES = 512 * 1024

    try:
        if HAS_REQUESTS:
            session = requests.Session()  # type: ignore[union-attr]
            session.auth = (user, password)
            session.verify = ssl_verify
            session.headers.update({"Content-Type": "application/json"})
            resp = session.get(url, timeout=timeout)
            status = resp.status_code
            content = resp.content
            text = resp.text
            if len(content) > MAX_BYTES:
                msg = f"response too large ({len(content)} bytes > {MAX_BYTES})"
                print(f"error: {msg}", file=sys.stderr)
                if args.json:
                    print(json.dumps({"ok": False, "error": msg, "host": host, "url": url, "status": status}))
                else:
                    print(f"Live FAIL: {msg} status={status}")
                sys.exit(4)
            if status == 200:
                try:
                    data = resp.json()
                except Exception:
                    # Try manual parse if json fails
                    try:
                        data = json.loads(text)
                    except Exception as e:
                        msg = f"parse error: {e}"
                        print(f"error: {msg}", file=sys.stderr)
                        if args.json:
                            print(json.dumps({"ok": False, "error": msg, "status": status, "host": host}))
                        else:
                            print(f"Live FAIL: {msg} status={status}")
                        sys.exit(4)
                # Count items
                if isinstance(data, list):
                    count = len(data)
                    # Enforce 500 cap note
                    if count > 500:
                        count_capped = 500
                    else:
                        count_capped = count
                    if args.json:
                        print(json.dumps({"ok": True, "host": host, "url": url, "scheme": scheme, "port": port, "count": count, "capped": count_capped if count > 500 else None, "status": status}))
                    else:
                        print(f"Live OK: {count} interfaces")
                        if count > 500:
                            print(f"(capped at 500 for display)")
                    sys.exit(0)
                elif isinstance(data, dict) and "error" in data:
                    msg = f"api error: {data.get('error')}"
                    print(f"error: {msg}", file=sys.stderr)
                    if args.json:
                        print(json.dumps({"ok": False, "error": msg, "status": status, "host": host}))
                    else:
                        print(f"Live FAIL: {msg} status={status}")
                    sys.exit(4)
                else:
                    # Unexpected shape but treat as success with 0?
                    msg = f"unexpected JSON shape: {type(data).__name__}"
                    print(f"warning: {msg}", file=sys.stderr)
                    if args.json:
                        print(json.dumps({"ok": True, "host": host, "url": url, "status": status, "count": 0, "warning": msg}))
                    else:
                        print(f"Live OK: 0 interfaces (unexpected shape)")
                    sys.exit(0)
            else:
                # Auth or other error
                body_preview = text[:500].replace("\n", " ")
                msg = f"http status {status}"
                print(f"error: {msg}: {body_preview}", file=sys.stderr)
                if args.json:
                    print(json.dumps({"ok": False, "error": msg, "status": status, "host": host, "url": url, "body_preview": body_preview[:200]}))
                else:
                    print(f"Live FAIL: {msg} {body_preview[:200]}")
                sys.exit(4)
        else:
            # Fallback urllib
            import urllib.request
            import urllib.error
            import ssl
            import base64

            req = urllib.request.Request(url, method="GET")
            creds = f"{user}:{password}".encode("utf-8")
            b64 = base64.b64encode(creds).decode("ascii")
            req.add_header("Authorization", f"Basic {b64}")
            req.add_header("Content-Type", "application/json")
            # SSL context
            ctx = None
            if scheme == "https" and not ssl_verify:
                ctx = ssl._create_unverified_context()
            try:
                with urllib.request.urlopen(req, timeout=timeout, context=ctx) as r:
                    status = r.status
                    content = r.read()
                    text = content.decode("utf-8", errors="replace")
                    if len(content) > MAX_BYTES:
                        msg = f"response too large ({len(content)} bytes > {MAX_BYTES})"
                        print(f"error: {msg}", file=sys.stderr)
                        if args.json:
                            print(json.dumps({"ok": False, "error": msg, "host": host, "url": url, "status": status}))
                        else:
                            print(f"Live FAIL: {msg} status={status}")
                        sys.exit(4)
                    if status == 200:
                        try:
                            data = json.loads(text)
                        except Exception as e:
                            msg = f"parse error: {e}"
                            print(f"error: {msg}", file=sys.stderr)
                            if args.json:
                                print(json.dumps({"ok": False, "error": msg, "status": status}))
                            else:
                                print(f"Live FAIL: {msg} status={status}")
                            sys.exit(4)
                        if isinstance(data, list):
                            count = len(data)
                            if args.json:
                                print(json.dumps({"ok": True, "host": host, "url": url, "scheme": scheme, "port": port, "count": count, "status": status}))
                            else:
                                print(f"Live OK: {count} interfaces")
                            sys.exit(0)
                        else:
                            if args.json:
                                print(json.dumps({"ok": True, "host": host, "url": url, "status": status, "count": 0}))
                            else:
                                print("Live OK: 0 interfaces (unexpected shape)")
                            sys.exit(0)
                    else:
                        body_preview = text[:500].replace("\n", " ")
                        msg = f"http status {status}"
                        print(f"error: {msg}: {body_preview}", file=sys.stderr)
                        if args.json:
                            print(json.dumps({"ok": False, "error": msg, "status": status, "host": host}))
                        else:
                            print(f"Live FAIL: {msg}")
                        sys.exit(4)
            except urllib.error.HTTPError as e:
                status = e.code
                try:
                    body = e.read().decode("utf-8", errors="replace")[:500]
                except Exception:
                    body = ""
                msg = f"http status {status}"
                print(f"error: {msg}: {body}", file=sys.stderr)
                if args.json:
                    print(json.dumps({"ok": False, "error": msg, "status": status, "host": host, "url": url}))
                else:
                    print(f"Live FAIL: {msg} {body[:200]}")
                sys.exit(4)
            except Exception as e:
                msg = f"network error: {e}"
                print(f"error: {msg}", file=sys.stderr)
                if args.json:
                    print(json.dumps({"ok": False, "error": msg, "host": host, "url": url}))
                else:
                    print(f"Live FAIL: {msg}")
                sys.exit(4)
    except SystemExit:
        raise
    except Exception as e:
        # Ensure never leak password
        msg = f"network error: {e}"
        # Strip any password accidentally in message (paranoid)
        if password and password in msg:
            msg = msg.replace(password, "[REDACTED]")
        print(f"error: {msg}", file=sys.stderr)
        if args.json:
            # Never include password
            print(json.dumps({"ok": False, "error": msg, "host": host, "url": url}))
        else:
            print(f"Live FAIL: {msg}")
        sys.exit(4)


if __name__ == "__main__":
    main()
