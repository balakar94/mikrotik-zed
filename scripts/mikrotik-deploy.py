#!/usr/bin/env python3
"""
MikroTik RSC deploy companion — push .rsc files to a RouterOS device.

Supports two transports:
  1) REST API via `requests` (preferred, RouterOS 7.20+ has /rest)
  2) SSH via `paramiko` (fallback, or explicit --method ssh)

Env vars (all can be overridden by CLI flags, mirrored in lsp/src/live_config.rs LiveConfig::from_env):
  MIKROTIK_HOST   - device host/IP (required)
  MIKROTIK_USER   - username (default: admin)
  MIKROTIK_PASS   - password (required)
  MIKROTIK_PORT   - REST 443 / SSH 22 (auto; live defaults to 443)
  MIKROTIK_SSL    - "0" to disable SSL certificate verification (REST);
                    verification only — it NEVER selects the URL scheme
  MIKROTIK_METHOD - "rest" or "ssh" (default: auto; live uses REST only)
  MIKROTIK_HTTP   - "1" to force plain HTTP for REST transport (default: https)
  MIKROTIK_TIMEOUT - per-request REST timeout and seconds to wait for the remote SSH /import (default: 60, clamped 1..300; live defaults to 5, clamped 1..30; SSH connect stays at fixed 15s)
  MIKROTIK_ACCEPT_HOST_KEY - "1" to trust unknown SSH host keys (TOFU; deploy SSH only)
  MIKROTIK_FINGERPRINT - SPKI SHA256 pin for REST TLS (format sha256:<hex>)
  MIKROTIK_CA_FILE - custom CA bundle path for REST TLS

Import success caveat: HTTP 200 or SSH exit code 0 does NOT guarantee the
import succeeded. /import output is additionally scanned for high-confidence
RouterOS failure markers ("syntax error", "input does not match",
"bad command name", "failure:") and treated as failed on a match. Direct
/rest/execute script output is printed verbatim and intentionally NOT scanned
(arbitrary scripts may legitimately echo such words).

Security note: REST/SSH targets ARE passed through
``_mikrotik_shared.validate_host`` (lexical SSRF denylist:
169.254.169.254 / metadata.google* / 0.0.0.0 / :: / 169.254.0.0/16)
before any network access, including ``--dry-run`` (validated first, then
previewed). Denied hosts exit 2 with no connection attempted. IPv6 hosts
are formatted with ``format_host_for_url`` before URL construction.

After the lexical gate, the pre-credential phase resolves the host exactly
once (``check_target_with_addrs``) and dials only the validated IPs: the
REST session pins them into a custom urllib3 connection class and SSH hands
a pre-connected socket to paramiko (``hostname`` stays the original for
known_hosts). No second DNS lookup can be rebound, closing the
resolve-then-connect TOCTOU. Transport itself stays HTTPS unless ``--http``
is explicit; ``--no-ssl-verify`` never changes the scheme.

Usage:
  python scripts/mikrotik-deploy.py path/to/file.rsc
  python scripts/mikrotik-deploy.py path/to/file.rsc --host 192.168.88.1 --user admin --dry-run
  MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret python scripts/mikrotik-deploy.py file.rsc

Zed tasks integration: see languages/rsc/tasks.json and README.md
"""
from __future__ import annotations

import argparse
import os
import re
import sys
import time
import pathlib
import getpass
import shlex
import urllib.parse

# Shared connection-setup helpers live in the sibling module. Make the
# scripts/ directory importable regardless of CWD or how this file is loaded
# (direct run as `python scripts/<name>.py`, or importlib in the test suite).
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from _mikrotik_shared import (  # noqa: E402
    build_pinned_requests_session,
    check_target_with_addrs,
    clamp_int,
    env_int,
    format_host_for_url,
    open_pinned_socket,
    parse_fingerprint,
    redact_secrets,
    resolve_scheme,
    validate_host,
    validate_user,
)

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

# Response cap for streamed REST bodies, matching live-check and
# lsp/src/caps.rs MAX_LIVE_RESPONSE_BYTES (512 KiB). Every device body is
# read through this bound and redacted before printing/logging.
MAX_RESPONSE_BYTES = 512 * 1024


def _match_import_failure_marker(output: str) -> str | None:
    """Return the first high-confidence failure marker found in /import output, or None."""
    lowered = output.lower()
    for marker in _IMPORT_FAILURE_MARKERS:
        if marker in lowered:
            return marker
    return None


def _read_response_capped(resp, password: str, user: str) -> str:
    """Read a streamed REST response with a 512 KiB cap, fail-closed.

    ``resp`` must have been requested with ``stream=True``. Reads at most
    ``MAX_RESPONSE_BYTES`` and exits 4 (after a redacted error) when the cap
    is exceeded or the stream errors. Returns decoded text; the caller is
    responsible for applying :func:`redact_secrets` before printing/logging.
    Never returns or logs credential material itself.
    """
    chunks: list[bytes] = []
    total = 0
    try:
        for chunk in resp.iter_content(chunk_size=8192):
            if chunk:
                total += len(chunk)
                if total > MAX_RESPONSE_BYTES:
                    msg = f"error: response too large ({total} bytes > {MAX_RESPONSE_BYTES})"
                    print(redact_secrets(msg, password, user), file=sys.stderr)
                    sys.exit(4)
                chunks.append(chunk)
    except Exception as e:
        # `requests` may be absent in a partial install; never reference its
        # exception class here (the caller only reaches this path when it is).
        print(
            redact_secrets(f"error: REST response read failed: {e}", password, user),
            file=sys.stderr,
        )
        sys.exit(4)
    content = b"".join(chunks)
    try:
        return content.decode(resp.encoding or "utf-8", errors="replace")
    except Exception:
        return content.decode("utf-8", errors="replace")


# Filename validation — mirrors lsp/src/live_net.rs::validate_host but adapted for filenames.
# Policy: safe filename for RouterOS file storage and REST URL path segment.
_FILENAME_RE = re.compile(r"^[a-zA-Z0-9._-]+$")


def validate_filename(filename: str) -> str | None:
    """Validate a remote filename for SFTP/REST file operations.

    Returns None on success, error string on failure.
    Checks: non-empty, 1..64 chars, no null/control, no path separators,
    no URI delimiters (%?#@), no parent segment '..', and strict charset
    ^[a-zA-Z0-9._-]+$ (same allowlist style as live_net.rs validate_host).
    """
    if not filename:
        return "empty"
    if len(filename) > 64:
        return "exceeds 64 characters"
    if "\0" in filename:
        return "contains null byte"
    # Control characters: ord < 32 or DEL (127), also covers \n \r \t
    if any(ord(c) < 32 or ord(c) == 127 for c in filename):
        return "contains control characters"
    # Path separators — never allow directory traversal
    if "/" in filename or "\\" in filename:
        return "contains path separator (/ or \\)"
    # '/' and '\\' are already rejected above, so only the exact '..'
    # filename remains reachable; keep it explicitly (it would otherwise
    # pass the allowlist below).
    if filename == "..":
        return "contains parent directory segment '..'"
    # URI delimiters that could alter URL parsing if interpolated
    if "%" in filename or "?" in filename or "#" in filename or "@" in filename:
        return "contains URI delimiter (%?#@)"
    if not _FILENAME_RE.match(filename):
        return "contains invalid characters (allowed: a-z, A-Z, 0-9, ., _, -)"
    return None


def _sanitize_and_validate_filename(raw: str) -> str:
    """Validate filename and exit 2 with a clear message on failure (never logs pass)."""
    err = validate_filename(raw)
    if err:
        print(f"error: invalid filename {raw!r}: {err}", file=sys.stderr)
        print(
            "hint: filename must match ^[a-zA-Z0-9._-]+$ and be 1..64 chars, no path separators or URI delimiters",
            file=sys.stderr,
        )
        sys.exit(2)
    return raw


def _deny_ssrf_host_or_exit(host: str) -> None:
    """Validate ``host`` against the shared SSRF denylist; exit 2 on denial."""
    err = validate_host(host)
    if err:
        print(f"error: invalid host {host!r}: {err}", file=sys.stderr)
        sys.exit(2)


def deploy_via_rest(host: str, user: str, password: str, port: int, ssl_verify: bool, content: str, filename: str, dry_run: bool, force_http: bool = False, timeout: int = 30, fingerprint: bytes | None = None, ca_file: str = "") -> None:
    # SSRF gate before any URL construction, logging, or network access —
    # enforced even on --dry-run.
    _deny_ssrf_host_or_exit(host)
    host_for_url = format_host_for_url(host)
    scheme = resolve_scheme(port, force_http)
    # Sanitize filename before any URL construction — same gate for both transports.
    filename = _sanitize_and_validate_filename(filename)
    # URL-encode validated filename for REST path segment (safe after allowlist).
    encoded_filename = urllib.parse.quote(filename, safe="")
    # Clamp per-request timeout to 1..300s (mirrors live 1..30 clamp, wider for file upload).
    effective_timeout = clamp_int(timeout, 1, 300, 30)
    if dry_run:
        log(f"DRY-RUN REST: would POST {len(content)} bytes to {scheme}://{host_for_url}:{port}/rest/execute as {user} (primary: direct execute)")
        log(f"DRY-RUN REST: fallback would PUT {len(content)} bytes to {scheme}://{host_for_url}:{port}/rest/file/{encoded_filename} as {user}")
        log(f"DRY-RUN REST: fallback would POST to {scheme}://{host_for_url}:{port}/rest/execute {{script: /import file={filename}}}")
        return
    if not HAS_REQUESTS:
        print("error: REST method requires 'requests' (pip install requests)", file=sys.stderr)
        sys.exit(3)
    base = f"{scheme}://{host_for_url}:{port}"
    # F1: lexical + resolve-then-revalidate before Authorization is sent:
    # lexical checks ran at startup, DNS may resolve differently now. Fail
    # closed. The validated addresses are pinned into the session adapter, so
    # the Authorization-carrying connection dials exactly those IPs with no
    # second DNS lookup.
    target_err, addrs = check_target_with_addrs(host, port)
    if target_err:
        print(f"error: {target_err}", file=sys.stderr)
        sys.exit(4)

    session = build_pinned_requests_session(
        user, password, addrs, ca_file, ssl_verify, fingerprint, scheme
    )

    # 1) Upload file content via /rest/file - RouterOS expects multipart or raw?
    # Fallback: use /rest/execute to run script directly without file
    # We try direct execute: POST /rest/execute with {"script": content}
    # This avoids file handling differences across versions.
    # Redirects are disabled on every call: 3xx fails closed, never followed.
    log(f"REST: uploading {len(content)} bytes to {host} as {user} (direct execute)")
    # The SPKI pin (when set) is verified inside the pinned HTTPS connection,
    # on the same socket as the request and before the Authorization header is
    # written — no separate handshake to race (see _mikrotik_shared).
    try:
        # Try direct execute. stream=True + _read_response_capped bounds the
        # body (an unbounded body read could OOM on a hostile/broken device).
        resp = session.post(f"{base}/rest/execute", json={"script": content}, timeout=effective_timeout, allow_redirects=False, stream=True)
        if resp.status_code in (200, 201, 204):
            log(f"REST: execute OK ({resp.status_code})")
            body = _read_response_capped(resp, password, user)
            if body and body.strip():
                print(redact_secrets(body, password, user))
            return
        if 300 <= resp.status_code < 400:
            print(f"error: redirect blocked (status {resp.status_code}); refusing to follow", file=sys.stderr)
            sys.exit(4)
        # If execute not allowed, try file method
        resp_body = _read_response_capped(resp, password, user)
        log(
            f"REST execute returned {resp.status_code}: "
            f"{redact_secrets(resp_body[:500], password, user)}"
        )
        log("REST: falling back to PUT /rest/file upload — EXPERIMENTAL: RouterOS's file API varies across versions")
        # File upload via /rest/file (PUT) — filename already validated and URL-encoded.
        # RouterOS file API is not well documented; we try PUT with contents field
        put_resp = session.put(f"{base}/rest/file/{encoded_filename}", json={"contents": content}, timeout=effective_timeout, allow_redirects=False, stream=True)
        if put_resp.status_code in (200, 201, 204):
            log(f"REST: file upload OK ({put_resp.status_code}), now importing")
            # RouterOS console accepts single-quoted strings; quoting guards
            # filenames containing spaces/special chars.
            imp = session.post(f"{base}/rest/execute", json={"script": f"/import file={shlex.quote(filename)}"}, timeout=effective_timeout, allow_redirects=False, stream=True)
            if 300 <= imp.status_code < 400:
                print(f"error: redirect blocked (status {imp.status_code}); refusing to follow", file=sys.stderr)
                sys.exit(4)
            imp_body = _read_response_capped(imp, password, user)
            log(
                f"REST: import result {imp.status_code}: "
                f"{redact_secrets(imp_body[:1000], password, user)}"
            )
            marker = _match_import_failure_marker(imp_body)
            if marker:
                print(
                    redact_secrets(
                        f"error: REST import failed (failure marker {marker!r}): {imp_body[:1000]}",
                        password,
                        user,
                    ),
                    file=sys.stderr,
                )
                sys.exit(5)
            return
        put_body = _read_response_capped(put_resp, password, user)
        msg = f"error: REST deploy failed: execute={resp.status_code} {resp_body[:1000]} file={put_resp.status_code} {put_body[:1000]}"
        print(redact_secrets(msg, password, user), file=sys.stderr)
        sys.exit(4)
    except requests.exceptions.RequestException as e:
        print(redact_secrets(f"error: REST request failed: {e}", password, user), file=sys.stderr)
        sys.exit(4)


def deploy_via_ssh(host: str, user: str, password: str, port: int, content: str, filename: str, dry_run: bool, accept_host_key: bool, timeout: int = 60) -> None:
    # SSRF gate before any SSH dial — enforced even on --dry-run.
    _deny_ssrf_host_or_exit(host)
    # Sanitize filename before SFTP — same gate as REST.
    filename = _sanitize_and_validate_filename(filename)
    if dry_run:
        log(f"DRY-RUN SSH: would scp {len(content)} bytes to {host}:{port} as {user} -> /{filename}")
        log(f"DRY-RUN SSH: would ssh {user}@{host} \"/import file={filename}\"")
        return
    if not HAS_PARAMIKO:
        print("error: SSH method requires 'paramiko' (pip install paramiko)", file=sys.stderr)
        sys.exit(3)
    # F1: lexical + resolve-then-revalidate before the password goes over
    # the wire — same pre-credential phase as REST. Dry-run already
    # returned above, so previews never touch the network.
    target_err, addrs = check_target_with_addrs(host, port)
    if target_err:
        print(f"error: {target_err}", file=sys.stderr)
        sys.exit(4)

    log(f"SSH: connecting to {host}:{port} as {user}")
    client = paramiko.SSHClient()
    # Load the user's known_hosts; unknown hosts are rejected by paramiko's default policy.
    client.load_system_host_keys()
    if accept_host_key:
        log("SSH: --accept-host-key active: unknown host keys will be trusted (MITM risk)")
        client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    pinned_sock = None
    try:
        # Dial the already-validated IP directly and hand the socket to
        # paramiko; `hostname` stays the original hostname so known_hosts
        # lookup (and the --accept-host-key policy) is unchanged. This closes
        # the resolve-then-connect TOCTOU: no second DNS resolution happens.
        # SSH connect timeout stays at a fixed 15s (independent of --timeout,
        # which only bounds the remote /import poll below).
        pinned_sock = open_pinned_socket(addrs, port, 15)
        client.connect(hostname=host, sock=pinned_sock, username=user, password=password, look_for_keys=False, allow_agent=False, timeout=15)
    except Exception as e:
        if pinned_sock is not None:
            try:
                pinned_sock.close()
            except OSError:
                pass
        print(redact_secrets(f"error: SSH connect failed: {e}", password, user), file=sys.stderr)
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
        # Clamp to 1..300s like REST so --timeout 0 / negative values cannot
        # spin-loop and huge values cannot hang the session; non-numeric
        # input falls back to the deploy default with a warning.
        effective_timeout = clamp_int(timeout, 1, 300, 60)
        deadline = time.monotonic() + effective_timeout
        while not stdout.channel.exit_status_ready():
            if time.monotonic() >= deadline:
                print(f"error: remote /import timed out after {effective_timeout}s", file=sys.stderr)
                sys.exit(5)  # the finally block below closes the SSH client
            time.sleep(0.1)
        out_bytes = stdout.read(MAX_RESPONSE_BYTES + 1)
        err_bytes = stderr.read(MAX_RESPONSE_BYTES + 1)
        if len(out_bytes) > MAX_RESPONSE_BYTES or len(err_bytes) > MAX_RESPONSE_BYTES:
            print(
                f"error: remote import output too large (> {MAX_RESPONSE_BYTES} bytes)",
                file=sys.stderr,
            )
            sys.exit(5)
        out = out_bytes.decode(errors="replace")
        err = err_bytes.decode(errors="replace")
        # Exit status is ready by now, so recv_exit_status() returns immediately.
        exit_status = stdout.channel.recv_exit_status()
        # Device bodies are redacted before printing (central helper covers
        # the password and base64 Basic material).
        if out:
            print(redact_secrets(out, password, user))
        if err:
            print(redact_secrets(err, password, user), file=sys.stderr)
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
        default=env_int("MIKROTIK_TIMEOUT", 60),
        help="Per-request REST timeout and seconds to wait for the remote SSH /import (env MIKROTIK_TIMEOUT, default 60, clamped 1..300; SSH connect stays at fixed 15s)",
    )
    p.add_argument(
        "--accept-host-key",
        action="store_true",
        default=os.getenv("MIKROTIK_ACCEPT_HOST_KEY") == "1",
        help="Trust unknown SSH host keys (trust-on-first-use). WARNING: vulnerable to MITM. Env: MIKROTIK_ACCEPT_HOST_KEY=1",
    )
    p.add_argument("--dry-run", action="store_true", help="Show what would be done without connecting")
    p.add_argument("--filename", default=None, help="Remote filename (default: basename of file)")
    p.add_argument(
        "--fingerprint",
        default=os.getenv("MIKROTIK_FINGERPRINT"),
        help="SPKI SHA256 pin for REST TLS (env MIKROTIK_FINGERPRINT, format sha256:<hex>)",
    )
    p.add_argument(
        "--ca-file",
        default=os.getenv("MIKROTIK_CA_FILE"),
        help="Custom CA bundle path for REST TLS (env MIKROTIK_CA_FILE)",
    )
    return p.parse_args()


def main() -> None:
    args = parse_args()

    # Runtime warning when the password actually comes from argv: values
    # passed via --pass are visible in process listings. Prefer
    # MIKROTIK_PASS env or the getpass prompt.
    if args.password and any(a == "--pass" or a.startswith("--pass=") for a in sys.argv[1:]):
        print(
            "warning: password supplied via --pass argv is visible in process listings;"
            " prefer MIKROTIK_PASS env or interactive prompt",
            file=sys.stderr,
        )

    if not args.host:
        print("error: --host or MIKROTIK_HOST is required", file=sys.stderr)
        print("example: MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret python scripts/mikrotik-deploy.py file.rsc --dry-run", file=sys.stderr)
        sys.exit(2)

    # SSRF gate before any file handling preview, transport dispatch, or
    # network access — enforced even on --dry-run (no connection attempted).
    _deny_ssrf_host_or_exit(args.host)

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
    # Early filename validation before any transport dispatch — same policy as
    # deploy_via_rest / deploy_via_ssh inner gates (double-checked there).
    # Never logs password.
    err = validate_filename(filename)
    if err:
        print(f"error: invalid filename {filename!r}: {err}", file=sys.stderr)
        print(
            "hint: filename must match ^[a-zA-Z0-9._-]+$ and be 1..64 chars, no path separators or URI delimiters",
            file=sys.stderr,
        )
        sys.exit(2)

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

    # Username parity with live.rs validate_user (fallback admin + WARN).
    user_validated = validate_user(args.user or "admin")
    if user_validated is None:
        print(
            f"warning: invalid user {(args.user or '').strip()!r}, falling back to admin",
            file=sys.stderr,
        )
        args.user = "admin"
    else:
        args.user = user_validated

    # TLS pin / CA bundle (REST only; fail closed on malformed pin).
    ca_file = (args.ca_file or "").strip()
    fingerprint_raw = (args.fingerprint or "").strip()
    fingerprint = parse_fingerprint(fingerprint_raw) if fingerprint_raw else None
    if fingerprint_raw and fingerprint is None:
        print(
            "error: invalid MIKROTIK_FINGERPRINT (expected sha256:<64 hex chars>)",
            file=sys.stderr,
        )
        sys.exit(2)

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
        deploy_via_rest(args.host, args.user, args.password or "", port, ssl_verify, content, filename, args.dry_run, force_http=args.http, timeout=args.timeout, fingerprint=fingerprint, ca_file=ca_file)
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
