#!/usr/bin/env python3
"""
MikroTik RSC deploy companion — push .rsc files to a RouterOS device.

Supports two transports:
  1) REST API via `requests` (preferred, RouterOS 7.20+ has /rest)
  2) SSH via `paramiko` (fallback, or explicit --method ssh)

Env vars (all can be overridden by CLI flags, mirrored in lsp/src/live.rs LiveConfig::from_env):
  MIKROTIK_HOST   - device host/IP (required)
  MIKROTIK_USER   - username (default: admin)
  MIKROTIK_PASS   - password (required)
  MIKROTIK_PORT   - REST 443 / SSH 22 (auto; live defaults to 443)
  MIKROTIK_SSL    - "0" to disable SSL certificate verification (REST);
                    verification only — it NEVER selects the URL scheme
  MIKROTIK_METHOD - "rest" or "ssh" (default: auto; live uses REST only)
  MIKROTIK_HTTP   - "1" to force plain HTTP for REST transport (default: https)
  MIKROTIK_TIMEOUT - seconds to wait for the remote SSH /import (default: 60; live defaults to 5, clamped 1..30)
  MIKROTIK_ACCEPT_HOST_KEY - "1" to trust unknown SSH host keys (TOFU; deploy SSH only)

Import success caveat: HTTP 200 or SSH exit code 0 does NOT guarantee the
import succeeded. /import output is additionally scanned for high-confidence
RouterOS failure markers ("syntax error", "input does not match",
"bad command name", "failure:") and treated as failed on a match. Direct
/rest/execute script output is printed verbatim and intentionally NOT scanned
(arbitrary scripts may legitimately echo such words).

Usage:
  python scripts/mikrotik-deploy.py path/to/file.rsc
  python scripts/mikrotik-deploy.py path/to/file.rsc --host 192.168.88.1 --user admin --dry-run
  MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret python scripts/mikrotik-deploy.py file.rsc

Zed tasks integration: see languages/rsc/tasks.json and README.md
"""
from __future__ import annotations

import argparse
import os
import sys
import time
import pathlib
import getpass
import shlex

# Optional dependencies - imported lazily
try:
    import requests  # type: ignore
    HAS_REQUESTS = True
except ImportError:
    requests = None  # type: ignore
    HAS_REQUESTS = False

try:
    import paramiko  # type: ignore
    HAS_PARAMIKO = True
except ImportError:
    paramiko = None  # type: ignore
    HAS_PARAMIKO = False


def log(msg: str) -> None:
    print(f"[mikrotik-deploy] {msg}", file=sys.stderr if "error" in msg.lower() else sys.stdout)


def load_file(path: pathlib.Path) -> str:
    if not path.exists():
        print(f"error: file not found: {path}", file=sys.stderr)
        sys.exit(2)
    if path.stat().st_size > 5 * 1024 * 1024:
        print(f"error: file too large (>5MiB): {path}", file=sys.stderr)
        sys.exit(2)
    return path.read_text(encoding="utf-8", errors="replace")


# High-confidence RouterOS /import failure markers (lowercase, matched against
# lowercased output). Deliberately conservative: arbitrary script output may
# legitimately contain these words, so they are applied ONLY to /import
# results (SSH exec output and the REST fallback-file import response), never
# to direct /rest/execute script output.
_IMPORT_FAILURE_MARKERS = ("syntax error", "input does not match", "bad command name", "failure:")


def _match_import_failure_marker(output: str) -> str | None:
    """Return the first high-confidence failure marker found in /import output, or None."""
    lowered = output.lower()
    for marker in _IMPORT_FAILURE_MARKERS:
        if marker in lowered:
            return marker
    return None


def resolve_scheme(port: int, force_http: bool, no_ssl_verify: bool) -> tuple[str, bool]:
    """Resolve the REST URL scheme.

    Default is HTTPS on every port; plain HTTP requires an explicit opt-in via
    --http (or MIKROTIK_HTTP=1). SSL verification (--no-ssl-verify /
    MIKROTIK_SSL=0) only controls certificate validation, never the scheme.

    Legacy shim: --no-ssl-verify used to also force http:// on non-standard
    ports (anything outside 443/8729), which plain-HTTP-on-port-80 setups
    relied on. That observable behavior is preserved — with a warning — until
    those users migrate to --http.

    Returns (scheme, legacy_shim_fired).
    """
    if not force_http and no_ssl_verify and port not in (443, 8729):
        return "http", True
    return ("http" if force_http else "https"), False


def _env_int(name: str, default: int) -> int:
    """Read an integer env var, falling back to default with a warning on bad input."""
    raw = os.getenv(name)
    if raw is None or not raw.strip():
        return default
    try:
        return int(raw)
    except ValueError:
        print(f"warning: invalid {name}={raw!r}, using default {default}", file=sys.stderr)
        return default


def deploy_via_rest(host: str, user: str, password: str, port: int, ssl_verify: bool, content: str, filename: str, dry_run: bool, force_http: bool = False) -> None:
    no_ssl_verify = not ssl_verify
    scheme, legacy_shim = resolve_scheme(port, force_http, no_ssl_verify)
    if legacy_shim:
        print(
            "warning: --no-ssl-verify no longer selects the scheme;"
            " use --http (or MIKROTIK_HTTP=1) explicitly",
            file=sys.stderr,
        )
    if dry_run:
        log(f"DRY-RUN REST: would POST {len(content)} bytes to {scheme}://{host}:{port}/rest/file/{filename} as {user}")
        log(f"DRY-RUN REST: would POST to {scheme}://{host}:{port}/rest/execute {{script: /import file={filename}}}")
        return
    if not HAS_REQUESTS:
        print("error: REST method requires 'requests' (pip install requests)", file=sys.stderr)
        sys.exit(3)
    base = f"{scheme}://{host}:{port}"

    session = requests.Session()
    session.auth = (user, password)
    session.verify = ssl_verify
    session.headers.update({"Content-Type": "application/json"})

    # 1) Upload file content via /rest/file - RouterOS expects multipart or raw?
    # Fallback: use /rest/execute to run script directly without file
    # We try direct execute: POST /rest/execute with {"script": content}
    # This avoids file handling differences across versions.
    log(f"REST: uploading {len(content)} bytes to {host} as {user} (direct execute)")
    try:
        # Try direct execute
        resp = session.post(f"{base}/rest/execute", json={"script": content}, timeout=30)
        if resp.status_code in (200, 201, 204):
            log(f"REST: execute OK ({resp.status_code})")
            if resp.text and resp.text.strip():
                print(resp.text)
            return
        # If execute not allowed, try file method
        log(f"REST execute returned {resp.status_code}: {resp.text[:500]}")
        log("REST: falling back to PUT /rest/file upload — EXPERIMENTAL: RouterOS's file API varies across versions")
        # File upload via /rest/file (PUT)
        # RouterOS file API is not well documented; we try PUT with contents field
        put_resp = session.put(f"{base}/rest/file/{filename}", json={"contents": content}, timeout=30)
        if put_resp.status_code in (200, 201, 204):
            log(f"REST: file upload OK ({put_resp.status_code}), now importing")
            # RouterOS console accepts single-quoted strings; quoting guards
            # filenames containing spaces/special chars.
            imp = session.post(f"{base}/rest/execute", json={"script": f"/import file={shlex.quote(filename)}"}, timeout=30)
            log(f"REST: import result {imp.status_code}: {imp.text[:1000]}")
            marker = _match_import_failure_marker(imp.text)
            if marker:
                print(f"error: REST import failed (failure marker {marker!r}): {imp.text[:1000]}", file=sys.stderr)
                sys.exit(5)
            return
        print(f"error: REST deploy failed: execute={resp.status_code} {resp.text[:1000]} file={put_resp.status_code} {put_resp.text[:1000]}", file=sys.stderr)
        sys.exit(4)
    except requests.exceptions.RequestException as e:
        print(f"error: REST request failed: {e}", file=sys.stderr)
        sys.exit(4)


def deploy_via_ssh(host: str, user: str, password: str, port: int, content: str, filename: str, dry_run: bool, accept_host_key: bool, timeout: int = 60) -> None:
    if dry_run:
        log(f"DRY-RUN SSH: would scp {len(content)} bytes to {host}:{port} as {user} -> /{filename}")
        log(f"DRY-RUN SSH: would ssh {user}@{host} \"/import file={filename}\"")
        return
    if not HAS_PARAMIKO:
        print("error: SSH method requires 'paramiko' (pip install paramiko)", file=sys.stderr)
        sys.exit(3)

    log(f"SSH: connecting to {host}:{port} as {user}")
    client = paramiko.SSHClient()
    # Load the user's known_hosts; unknown hosts are rejected by paramiko's default policy.
    client.load_system_host_keys()
    if accept_host_key:
        log("SSH: --accept-host-key active: unknown host keys will be trusted (MITM risk)")
        client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    try:
        client.connect(hostname=host, port=port, username=user, password=password, look_for_keys=False, allow_agent=False, timeout=15)
    except Exception as e:
        print(f"error: SSH connect failed: {e}", file=sys.stderr)
        if not accept_host_key:
            print(
                "hint: the host key may be missing from known_hosts. After verifying the device fingerprint,"
                " retry with --accept-host-key (or MIKROTIK_ACCEPT_HOST_KEY=1).",
                file=sys.stderr,
            )
        sys.exit(4)

    try:
        sftp = client.open_sftp()
        log(f"SSH: uploading {filename} ({len(content)} bytes)")
        # paramiko SFTP expects bytes
        with sftp.file(filename, "w") as f:
            f.write(content)
        sftp.close()
        log("SSH: upload complete, running /import")
        # RouterOS console accepts single-quoted strings; quoting guards
        # filenames containing spaces/special chars.
        stdin, stdout, stderr = client.exec_command(f"/import file={shlex.quote(filename)}")
        # Poll for completion instead of calling recv_exit_status() directly,
        # which blocks forever if the device never terminates the /import.
        # Clamp to >=1s so --timeout 0 / negative values cannot spin-loop or
        # time out before the command is even dispatched.
        effective_timeout = max(int(timeout), 1)
        deadline = time.monotonic() + effective_timeout
        while not stdout.channel.exit_status_ready():
            if time.monotonic() >= deadline:
                print(f"error: remote /import timed out after {effective_timeout}s", file=sys.stderr)
                sys.exit(5)  # the finally block below closes the SSH client
            time.sleep(0.1)
        out = stdout.read().decode(errors="replace")
        err = stderr.read().decode(errors="replace")
        # Exit status is ready by now, so recv_exit_status() returns immediately.
        exit_status = stdout.channel.recv_exit_status()
        if out:
            print(out)
        if err:
            print(err, file=sys.stderr)
        if exit_status != 0:
            print(f"error: remote import failed with exit {exit_status}", file=sys.stderr)
            sys.exit(5)
        marker = _match_import_failure_marker(f"{out}\n{err}")
        if marker:
            print(f"error: remote import failed (failure marker {marker!r})", file=sys.stderr)
            sys.exit(5)
        log("SSH: import OK")
    finally:
        client.close()


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Deploy .rsc file to MikroTik RouterOS")
    p.add_argument("file", help="Path to .rsc file")
    p.add_argument("--host", default=os.getenv("MIKROTIK_HOST"), help="Device host/IP (env MIKROTIK_HOST)")
    p.add_argument("--user", default=os.getenv("MIKROTIK_USER", "admin"), help="Username (env MIKROTIK_USER, default admin)")
    p.add_argument("--pass", dest="password", default=os.getenv("MIKROTIK_PASS"), help="Password (env MIKROTIK_PASS). Prefer env/getpass: values passed via argv are visible in process listings")
    p.add_argument("--port", type=int, default=None, help="Port (env MIKROTIK_PORT, default 443 for REST, 22 for SSH)")
    p.add_argument("--method", choices=["auto", "rest", "ssh"], default=os.getenv("MIKROTIK_METHOD", "auto"), help="Transport: rest, ssh, auto (default auto)")
    p.add_argument("--no-ssl-verify", action="store_true", default=os.getenv("MIKROTIK_SSL") == "0", help="Disable SSL certificate verification for REST (does not change the URL scheme)")
    p.add_argument(
        "--http",
        action="store_true",
        default=os.getenv("MIKROTIK_HTTP") == "1",
        help="Force plain HTTP for REST transport (env MIKROTIK_HTTP=1)",
    )
    p.add_argument(
        "--timeout",
        type=int,
        default=_env_int("MIKROTIK_TIMEOUT", 60),
        help="Seconds to wait for the remote /import to finish, SSH (env MIKROTIK_TIMEOUT, default 60)",
    )
    p.add_argument(
        "--accept-host-key",
        action="store_true",
        default=os.getenv("MIKROTIK_ACCEPT_HOST_KEY") == "1",
        help="Trust unknown SSH host keys (trust-on-first-use). WARNING: vulnerable to MITM. Env: MIKROTIK_ACCEPT_HOST_KEY=1",
    )
    p.add_argument("--dry-run", action="store_true", help="Show what would be done without connecting")
    p.add_argument("--filename", default=None, help="Remote filename (default: basename of file)")
    return p.parse_args()


def main() -> None:
    args = parse_args()

    if not args.host:
        print("error: --host or MIKROTIK_HOST is required", file=sys.stderr)
        print("example: MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret python scripts/mikrotik-deploy.py file.rsc --dry-run", file=sys.stderr)
        sys.exit(2)

    if not args.password and not args.dry_run:
        # Prompt securely if not provided and not dry-run
        try:
            args.password = getpass.getpass(f"Password for {args.user}@{args.host}: ")
        except Exception:
            pass
        if not args.password:
            print("error: --pass or MIKROTIK_PASS is required", file=sys.stderr)
            sys.exit(2)

    path = pathlib.Path(args.file)
    content = load_file(path)
    filename = args.filename or path.name

    # Basic validation: RSC files should contain RouterOS commands
    if not content.strip():
        print(f"error: file is empty: {path}", file=sys.stderr)
        sys.exit(2)
    if args.dry_run:
        log(f"DRY-RUN: {path} -> {args.host} as {args.user} ({len(content)} bytes, method={args.method})")
        # Show first 500 chars
        preview = content[:500].replace("\n", "\\n")
        log(f"Preview: {preview[:200]}...")

    method = args.method
    port = args.port
    if port is None:
        if method == "ssh":
            port = 22
        elif method == "rest":
            port = 443
        else:
            # auto: prefer REST, so 443
            port = int(os.getenv("MIKROTIK_PORT", "443")) if os.getenv("MIKROTIK_PORT") else 443

    ssl_verify = not args.no_ssl_verify

    # Auto selection (allow dry-run without deps)
    if method == "auto":
        if args.dry_run:
            # Prefer rest for dry-run preview, even if no libs installed
            method = "rest"
        elif HAS_REQUESTS:
            method = "rest"
        elif HAS_PARAMIKO:
            method = "ssh"
        else:
            print("error: no transport available: install 'requests' or 'paramiko' (pip install requests paramiko)", file=sys.stderr)
            sys.exit(3)

    if method == "rest":
        deploy_via_rest(args.host, args.user, args.password or "", port, ssl_verify, content, filename, args.dry_run, force_http=args.http)
    elif method == "ssh":
        # For SSH, default port 22 if auto gave 443
        if args.port is None and port == 443:
            port = 22
        deploy_via_ssh(args.host, args.user, args.password or "", port, content, filename, args.dry_run, args.accept_host_key, timeout=args.timeout)
    else:
        print(f"error: unknown method {method}", file=sys.stderr)
        sys.exit(2)

    log("Done.")


if __name__ == "__main__":
    main()
