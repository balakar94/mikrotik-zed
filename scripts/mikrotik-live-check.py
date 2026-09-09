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
  MIKROTIK_FINGERPRINT - SPKI SHA256 pin (format sha256:<hex>)
  MIKROTIK_CA_FILE - custom CA bundle path

The check performs a real authenticated GET to /rest/interface and reports
item count. It never prints the password. Dry-run mode shows what would be
called without connecting.

Usage:
  python scripts/mikrotik-live-check.py --host 192.168.88.1 --user admin
  python scripts/mikrotik-live-check.py --dry-run
  MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret python scripts/mikrotik-live-check.py
  MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret python scripts/mikrotik-live-check.py --json

Exit codes:
  0 - Live OK (reachable, valid JSON list)
  2 - Usage error (missing host)
  4 - Live FAIL (network, auth, status, parse, unexpected JSON shape, or host validation failure)
"""

from __future__ import annotations

import argparse
import getpass
import json
import os
import pathlib
import sys

# Shared connection-setup helpers live in the sibling module. Make the
# scripts/ directory importable regardless of CWD or how this file is loaded
# (direct run as `python scripts/<name>.py`, or importlib in the test suite).
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from _mikrotik_shared import (  # noqa: E402
    env_int,
    format_host_for_url,
    parse_fingerprint,
    redact_secrets,
    resolve_and_check_host,
    resolve_scheme,
    validate_host,
    validate_user,
    verify_tls_pin,
)

# Optional requests - fallback to urllib
try:
    import requests  # type: ignore

    HAS_REQUESTS = True
except ImportError:
    requests = None  # type: ignore
    HAS_REQUESTS = False


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
        default=env_int("MIKROTIK_TIMEOUT", 5),
        help="Request timeout seconds (env MIKROTIK_TIMEOUT, default 5, clamped 1..30)",
    )
    p.add_argument("--json", action="store_true", help="Output JSON instead of human text")
    p.add_argument("--dry-run", action="store_true", help="Show what would be called without connecting")
    p.add_argument(
        "--fingerprint",
        default=os.getenv("MIKROTIK_FINGERPRINT"),
        help="SPKI SHA256 pin (env MIKROTIK_FINGERPRINT, format sha256:<hex>)",
    )
    p.add_argument(
        "--ca-file",
        default=os.getenv("MIKROTIK_CA_FILE"),
        help="Custom CA bundle path (env MIKROTIK_CA_FILE)",
    )
    # Compatibility shim for tasks.json that still passes --method rest (ignored, but required for test)
    p.add_argument("--method", choices=["rest", "auto", "ssh"], default="rest", help=argparse.SUPPRESS)
    return p.parse_args()


def main() -> None:
    args = parse_args()

    host = (args.host or "").strip()
    user_raw = args.user or "admin"
    user = validate_user(user_raw) or "admin"
    if user != (user_raw or "").strip():
        print(
            f"warning: invalid user {(user_raw or '').strip()!r}, falling back to admin",
            file=sys.stderr,
        )

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
    ca_file = (args.ca_file or "").strip()
    fingerprint_raw = (args.fingerprint or "").strip()
    fingerprint = parse_fingerprint(fingerprint_raw) if fingerprint_raw else None
    if fingerprint_raw and fingerprint is None:
        msg = "invalid MIKROTIK_FINGERPRINT (expected sha256:<64 hex chars>)"
        print(f"error: {msg}", file=sys.stderr)
        if args.json:
            print(json.dumps({"ok": False, "error": msg, "host": host}))
        else:
            print(f"Live FAIL: {msg}")
        sys.exit(4)

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
                        "fingerprint_set": fingerprint is not None,
                        "ca_file_set": bool(ca_file),
                    }
                )
            )
        else:
            print(f"[mikrotik-live-check] DRY-RUN: would GET {url} as {user} (ssl_verify={ssl_verify}, timeout={timeout}s, method={args.method})")
            print(f"DRY-RUN: host={host} port={port} scheme={scheme} user={user} url={url}")
        sys.exit(0)

    # F1: resolve-then-revalidate before any credential use (fail fast, no
    # prompt when DNS already refuses). Lexical checks ran at startup; DNS
    # may resolve differently now. verify_tls_pin re-checks on its own
    # handshake when a pin is set.
    dns_err = resolve_and_check_host(host, port)
    if dns_err:
        msg = dns_err
        print(f"error: {msg}", file=sys.stderr)
        if args.json:
            print(json.dumps({"ok": False, "error": msg, "host": host, "url": url}))
        else:
            print(f"Live FAIL: {msg}")
        sys.exit(4)

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
            # Custom CA bundle wins over the boolean flag; a pin still
            # enforces SPKI matching on top (verified below pre-parse).
            # F3: when a pin is configured, the HTTP connection itself must
            # verify (the pin check runs on a SEPARATE handshake — TOCTOU).
            # Never CERT_NONE with Authorization when a pin is set.
            if fingerprint is not None and scheme == "https":
                session.verify = ca_file if ca_file else True
            else:
                session.verify = ca_file if ca_file else ssl_verify
            session.headers.update({"Content-Type": "application/json"})
            # Streamed read with byte cap — prevents OOM on unbounded responses
            # (same 512 KiB limit as caps.rs MAX_LIVE_RESPONSE_BYTES).
            # Redirects are disabled: 3xx is a fail-closed error, never followed.
            try:
                resp = session.get(url, timeout=timeout, stream=True, allow_redirects=False)
            except requests.exceptions.Timeout as e:  # type: ignore[union-attr]
                msg = f"request timed out: {e}"
                print(f"error: {msg}", file=sys.stderr)
                if args.json:
                    print(json.dumps({"ok": False, "error": msg, "host": host, "url": url}))
                else:
                    print(f"Live FAIL: {msg}")
                sys.exit(4)
            except requests.exceptions.RequestException as e:  # type: ignore[union-attr]
                msg = redact_secrets(f"network error: {e}", password, user)
                print(f"error: {msg}", file=sys.stderr)
                if args.json:
                    print(json.dumps({"ok": False, "error": msg, "host": host, "url": url}))
                else:
                    print(f"Live FAIL: {msg}")
                sys.exit(4)
            status = resp.status_code
            if 300 <= status < 400:
                msg = f"redirect blocked (status {status}); refusing to follow"
                print(f"error: {msg}", file=sys.stderr)
                if args.json:
                    print(json.dumps({"ok": False, "error": msg, "host": host, "url": url, "status": status}))
                else:
                    print(f"Live FAIL: {msg} status={status}")
                sys.exit(4)
            if fingerprint is not None and scheme == "https":
                pin_err = verify_tls_pin(host, port, fingerprint, ca_file, timeout, ssl_verify)
                if pin_err:
                    msg = redact_secrets(pin_err, password, user)
                    print(f"error: {msg}", file=sys.stderr)
                    if args.json:
                        print(json.dumps({"ok": False, "error": msg, "host": host, "url": url}))
                    else:
                        print(f"Live FAIL: {msg}")
                    sys.exit(4)
            # Incremental streaming read — abort if exceeds MAX_BYTES
            content_chunks: list[bytes] = []
            total = 0
            try:
                for chunk in resp.iter_content(chunk_size=8192):
                    if chunk:
                        total += len(chunk)
                        if total > MAX_BYTES:
                            msg = f"response too large ({total} bytes > {MAX_BYTES})"
                            print(f"error: {msg}", file=sys.stderr)
                            if args.json:
                                print(json.dumps({"ok": False, "error": msg, "host": host, "url": url, "status": status}))
                            else:
                                print(f"Live FAIL: {msg} status={status}")
                            sys.exit(4)
                        content_chunks.append(chunk)
            except requests.exceptions.RequestException as e:  # type: ignore[union-attr]
                msg = redact_secrets(f"network error during streaming: {e}", password, user)
                print(f"error: {msg}", file=sys.stderr)
                if args.json:
                    print(json.dumps({"ok": False, "error": msg, "host": host, "url": url}))
                else:
                    print(f"Live FAIL: {msg}")
                sys.exit(4)
            content = b"".join(content_chunks)
            # Respect server encoding but fall back to utf-8
            try:
                text = content.decode(resp.encoding or "utf-8", errors="replace")
            except Exception:
                text = content.decode("utf-8", errors="replace")
            # Already streaming-capped above; no second size check needed
            if status == 200:
                # Parse from our streamed buffer (resp.json() would re-read the
                # already-drained stream). Use text directly to avoid OOM and
                # double-read.
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
                    # Unexpected shape: fail closed — a health check must not
                    # report OK on a payload it cannot understand.
                    msg = f"unexpected JSON shape: {type(data).__name__} (expected a list)"
                    print(f"error: {msg}", file=sys.stderr)
                    if args.json:
                        print(json.dumps({"ok": False, "error": msg, "status": status, "host": host, "url": url}))
                    else:
                        print(f"Live FAIL: {msg} status={status}")
                    sys.exit(4)
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
            # Fallback urllib (no redirects: fail closed on 3xx, never follow).
            import urllib.request
            import urllib.error
            import ssl
            import base64

            class _NoRedirect(urllib.request.HTTPRedirectHandler):
                def redirect_request(self, req, fp, code, msg, headers, newurl):
                    return None

            req = urllib.request.Request(url, method="GET")
            creds = f"{user}:{password}".encode("utf-8")
            b64 = base64.b64encode(creds).decode("ascii")
            req.add_header("Authorization", f"Basic {b64}")
            req.add_header("Content-Type", "application/json")
            # SSL context (custom CA bundle wins; pin verified below pre-parse).
            # F3: when a pin is configured, never CERT_NONE on the
            # Authorization-carrying connection (TOCTOU between the pin
            # handshake and this HTTP connection). Force verification.
            pin_enforced = fingerprint is not None and scheme == "https"
            ctx = None
            if scheme == "https":
                if ca_file:
                    ctx = ssl.create_default_context(cafile=ca_file)
                    if not ssl_verify and not pin_enforced:
                        ctx.check_hostname = False
                        ctx.verify_mode = ssl.CERT_NONE
                elif not ssl_verify and not pin_enforced:
                    ctx = ssl._create_unverified_context()
                elif pin_enforced:
                    ctx = ssl.create_default_context()
            handlers: list = [_NoRedirect]
            if ctx is not None:
                # OpenerDirector.open has no `context` kwarg; the context
                # travels on the HTTPSHandler instead.
                handlers.append(urllib.request.HTTPSHandler(context=ctx))
            opener = urllib.request.build_opener(*handlers)
            try:
                if fingerprint is not None and scheme == "https":
                    pin_err = verify_tls_pin(host, port, fingerprint, ca_file, timeout, ssl_verify)
                    if pin_err:
                        msg = redact_secrets(pin_err, password, user)
                        print(f"error: {msg}", file=sys.stderr)
                        if args.json:
                            print(json.dumps({"ok": False, "error": msg, "host": host, "url": url}))
                        else:
                            print(f"Live FAIL: {msg}")
                        sys.exit(4)
                with opener.open(req, timeout=timeout) as r:
                    status = r.status
                    # urllib does not support stream iteration like requests; still cap via limit
                    content = r.read(MAX_BYTES + 1)
                    # If we got more than MAX_BYTES, abort without reading the rest
                    # (read(MAX_BYTES+1) guarantees we detect overflow in one read)
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
                            # Unexpected shape: fail closed — a health check must
                            # not report OK on a payload it cannot understand.
                            msg = f"unexpected JSON shape: {type(data).__name__} (expected a list)"
                            print(f"error: {msg}", file=sys.stderr)
                            if args.json:
                                print(json.dumps({"ok": False, "error": msg, "status": status, "host": host, "url": url}))
                            else:
                                print(f"Live FAIL: {msg} status={status}")
                            sys.exit(4)
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
                if 300 <= status < 400:
                    msg = f"redirect blocked (status {status}); refusing to follow"
                    print(f"error: {msg}", file=sys.stderr)
                    if args.json:
                        print(json.dumps({"ok": False, "error": msg, "status": status, "host": host, "url": url}))
                    else:
                        print(f"Live FAIL: {msg} {status}")
                    sys.exit(4)
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
                msg = redact_secrets(f"network error: {e}", password, user)
                print(f"error: {msg}", file=sys.stderr)
                if args.json:
                    print(json.dumps({"ok": False, "error": msg, "host": host, "url": url}))
                else:
                    print(f"Live FAIL: {msg}")
                sys.exit(4)
    except SystemExit:
        raise
    except Exception as e:
        # Ensure never leak password (central helper covers base64 too).
        msg = redact_secrets(f"network error: {e}", password, user)
        print(f"error: {msg}", file=sys.stderr)
        if args.json:
            # Never include password
            print(json.dumps({"ok": False, "error": msg, "host": host, "url": url}))
        else:
            print(f"Live FAIL: {msg}")
        sys.exit(4)


if __name__ == "__main__":
    main()
