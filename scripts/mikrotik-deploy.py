#!/usr/bin/env python3
"""
MikroTik RSC deploy companion — push .rsc files to a RouterOS device.

Supports two transports:
  1) REST API via `requests` (preferred; RouterOS 7.1+ has /rest)
  2) SSH via `paramiko` (fallback, or explicit --method ssh)

Env vars (all can be overridden by CLI flags, mirrored in lsp/src/live_config.rs LiveConfig::from_env):
  MIKROTIK_HOST   - device host/IP (required; ONE host — comma-separated
                    multi-host is only supported by rsc-ls live, which fetches
                    the primary host only)
  MIKROTIK_USER   - username (default: admin)
  MIKROTIK_PASS   - password (required)
  MIKROTIK_PORT   - REST 443 / SSH 22 (auto; live defaults to 443)
  MIKROTIK_SSL    - "0" to disable SSL certificate verification (REST);
                    verification only — it NEVER selects the URL scheme
  MIKROTIK_METHOD - "rest" or "ssh" (default: auto; live uses REST only)
  MIKROTIK_HTTP   - "1" to force plain HTTP for REST transport (default: https)
  MIKROTIK_TIMEOUT - per-request REST timeout and seconds to wait for the remote SSH /import (default: 60, clamped 1..300; REST is additionally capped at the device's 60s server-side limit; live defaults to 5, clamped 1..30; SSH connect stays at fixed 15s)
  MIKROTIK_ACCEPT_HOST_KEY - "1" to trust unknown SSH host keys (TOFU; deploy SSH only)
  MIKROTIK_IDENTITY - private key path for SSH auth (default: password only)
  MIKROTIK_FINGERPRINT - SPKI SHA256 pin for REST TLS (format sha256:<hex>)
  MIKROTIK_CA_FILE - custom CA bundle path for REST TLS
  RSC_LS_LIVE_DENY_PREFIXES - comma-separated IPv4/IPv6 addresses or CIDR
                               prefixes always denied by the SSRF target check
                               (max 32 entries; env-only)

Import success caveat: HTTP 200 or SSH exit code 0 does NOT guarantee the
import succeeded. /import output is additionally scanned for high-confidence
RouterOS failure markers ("syntax error", "input does not match",
"bad command name", "failure:") and treated as failed on a match.

Direct /rest/execute deploy verification: the executed payload gets a final
`:put "RSC_DEPLOY_OK"` line, and the response must contain that sentinel.
RouterOS stops a script on the first error, so the sentinel proves the last
statement was reached; absence is treated as an uncertain result (exit 5,
device state must be verified). Use --no-verify-execute only for builds that
do not return /rest/execute output. The sentinel is skipped for payloads
ending in a line continuation (logged, result unverified).

Exit codes: 2 usage/validation/destructive refusal, 3 missing transport
dependency, 4 network/auth/HTTP failure, 5 import/execute failed or
unverified.

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
import base64
import hashlib
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

# Machine-checkable completion sentinel appended to the direct /rest/execute
# payload. RouterOS aborts a script on the first error, so the sentinel in
# the response proves the last statement was reached (DEP-01).
_REST_SENTINEL = "RSC_DEPLOY_OK"

# RouterOS documents file-content editing only up to 60 KB, so the REST
# file-upload fallback is offered below that bound; larger payloads must use
# --method ssh (DEP-02).
_REST_FILE_FALLBACK_MAX_BYTES = 60 * 1024

# RouterOS closes REST commands after 60 s server-side (documented limit), so
# a longer client timeout cannot extend a REST command (DEP-03).
_REST_SERVER_TIMEOUT_SECS = 60


def _strip_comments_and_strings(line: str) -> str:
    """Best-effort strip of RouterOS `#` comments and quoted strings.

    The destructive pre-scan must not fire on prose (`# remove old rules`) or
    on string literals (`comment="remove me"`). This is a character-level
    pass, not a parser: `#` starts a comment outside quotes; `\'`/`"` spans
    are skipped with backslash escapes (which also covers `$"..."`).
    """
    out: list[str] = []
    quote: str | None = None
    i = 0
    n = len(line)
    while i < n:
        c = line[i]
        if quote is not None:
            if c == "\\" and i + 1 < n:
                i += 2
                continue
            if c == quote:
                quote = None
            i += 1
            continue
        if c == "#":
            break
        if c in ("'", '"'):
            quote = c
            i += 1
            continue
        out.append(c)
        i += 1
    return "".join(out)


# Hard-blocked resets vs confirmation-gated removals. `--force-destructive` is
# the single bypass for both (runs before any network access, including
# --dry-run).
_HARD_RESET_RE = re.compile(r"(^|[\s/])system\s+reset\b", re.IGNORECASE)
_RESET_CONFIG_RE = re.compile(r"\breset-configuration\b", re.IGNORECASE)
_REMOVE_RE = re.compile(r"\bremove\b", re.IGNORECASE)


def classify_destructive_line(line: str) -> str | None:
    """Classify one source line as "reset", "remove" or None (best-effort).

    Comments and quoted strings are stripped first, so prose cannot trip the
    gate. `system reset`/`reset-configuration` and `remove` are both gated;
    the caller distinguishes hard-block from confirmation in its message.
    """
    code = _strip_comments_and_strings(line)
    if _HARD_RESET_RE.search(code) or _RESET_CONFIG_RE.search(code):
        return "reset"
    if _REMOVE_RE.search(code):
        return "remove"
    return None


def find_destructive_lines(content: str) -> list[tuple[int, str]]:
    """Return (line number, line text) for lines matching the destructive scan.

    Best-effort: comments/strings are ignored, but dynamic construction
    (`:execute`, variable-held command names, `/tool fetch` + import) is not
    detected. The gate is a safety net, not a sandbox.
    """
    hits: list[tuple[int, str]] = []
    for lineno, line in enumerate((content or "").splitlines(), start=1):
        if classify_destructive_line(line) is not None:
            hits.append((lineno, line.strip()))
    return hits


def check_destructive_or_exit(content: str, force_destructive: bool) -> None:
    """Refuse destructive content unless explicitly acknowledged (exit 2).

    Best-effort lexical gate: hard-blocks `system reset`/`reset-configuration`
    and requires explicit confirmation (`--force-destructive`) for `remove`.
    Comments and quoted strings are ignored so prose cannot trip the scan.
    Prints the offending line numbers and a backup hint. Never logs secrets.
    """
    reset_hits: list[tuple[int, str]] = []
    remove_hits: list[tuple[int, str]] = []
    for lineno, line in enumerate((content or "").splitlines(), start=1):
        kind = classify_destructive_line(line)
        if kind == "reset":
            reset_hits.append((lineno, line.strip()))
        elif kind == "remove":
            remove_hits.append((lineno, line.strip()))
    if not reset_hits and not remove_hits:
        return
    if force_destructive:
        print(
            f"warning: destructive content acknowledged "
            f"({len(reset_hits)} reset, {len(remove_hits)} remove line(s)); proceeding",
            file=sys.stderr,
        )
        return
    shown = ", ".join(
        f"line {n}: {t[:80]}" for n, t in (reset_hits + remove_hits)[:5]
    )
    print(f"error: destructive content detected ({shown})", file=sys.stderr)
    if reset_hits:
        print(
            "hard-blocked: system reset / reset-configuration",
            file=sys.stderr,
        )
    if remove_hits:
        print(
            "requires explicit confirmation: remove",
            file=sys.stderr,
        )
    print(
        "hint: take /export file=pre-<ts> before push"
        " (e.g. /export file=pre-20260101-120000), or use --backup,"
        " then retry with --force-destructive",
        file=sys.stderr,
    )
    sys.exit(2)


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


def deploy_via_rest(
    host: str,
    user: str,
    password: str,
    port: int,
    ssl_verify: bool,
    content: str,
    filename: str,
    dry_run: bool,
    force_http: bool = False,
    timeout: int = 30,
    fingerprint: bytes | None = None,
    ca_file: str = "",
    force_destructive: bool = False,
    verify_execute: bool = True,
    keep_file: bool = False,
    backup: bool = False,
) -> None:
    # SSRF gate before any URL construction, logging, or network access —
    # enforced even on --dry-run.
    _deny_ssrf_host_or_exit(host)
    host_for_url = format_host_for_url(host)
    scheme = resolve_scheme(port, force_http)
    # Sanitize filename before any URL construction — same gate for both transports.
    filename = _sanitize_and_validate_filename(filename)
    # Local destructive pre-scan before any preview or network access.
    # Same verdict on dry-run (exit 2 unless --force-destructive).
    check_destructive_or_exit(content, force_destructive)
    # URL-encode validated filename for REST path segment (safe after allowlist).
    encoded_filename = urllib.parse.quote(filename, safe="")
    content_bytes = len(content.encode("utf-8"))
    # DEP-03: clamp to 1..300s first (CLI contract), then honor the device's
    # documented 60s REST server-side cap — a larger client timeout cannot
    # extend a REST command. --method ssh keeps the full 1..300s range.
    clamped_timeout = clamp_int(timeout, 1, 300, 30)
    effective_timeout = min(clamped_timeout, _REST_SERVER_TIMEOUT_SECS)
    if clamped_timeout > _REST_SERVER_TIMEOUT_SECS:
        log(
            f"warning: timeout {clamped_timeout}s exceeds the RouterOS REST "
            f"{_REST_SERVER_TIMEOUT_SECS}s server-side cap; using {effective_timeout}s for REST "
            "(use --method ssh for long imports)"
        )
    # DEP-01: append a completion sentinel to the executed payload. A payload
    # ending in a line continuation would swallow the sentinel, so that rare
    # case runs unverified with a warning; RouterOS stops a script on error,
    # so observing the sentinel proves the last statement was reached.
    trailing_continuation = content.rstrip().endswith("\\")
    verify_this = verify_execute and not trailing_continuation
    payload = content if not verify_this else f'{content}\n:put "{_REST_SENTINEL}"\n'
    if verify_execute and trailing_continuation:
        log(
            "warning: payload ends with a line continuation; execute-result "
            "verification skipped for this push"
        )
    if dry_run:
        log(f"DRY-RUN REST: would POST {content_bytes} bytes to {scheme}://{host_for_url}:{port}/rest/execute as {user} (primary: direct execute)")
        if verify_this:
            log(
                f'DRY-RUN REST: execute payload appends :put "{_REST_SENTINEL}"; '
                "its absence in the response is treated as failure (skip with --no-verify-execute)"
            )
        else:
            log("DRY-RUN REST: execute-result verification disabled for this push")
        if content_bytes > _REST_FILE_FALLBACK_MAX_BYTES:
            log(
                f"DRY-RUN REST: content exceeds {_REST_FILE_FALLBACK_MAX_BYTES} bytes; "
                "the /rest/file fallback would be unavailable (use --method ssh)"
            )
        log(f"DRY-RUN REST: fallback would PUT {content_bytes} bytes to {scheme}://{host_for_url}:{port}/rest/file/{encoded_filename} as {user}")
        log(f"DRY-RUN REST: fallback would POST to {scheme}://{host_for_url}:{port}/rest/execute {{script: /import file={filename}}}")
        if backup:
            log(f"DRY-RUN REST: would first POST {scheme}://{host_for_url}:{port}/rest/export {{file: pre-<utc-ts>}} (on-device backup)")
        if not keep_file:
            log("DRY-RUN REST: would DELETE the uploaded file after a successful import (--keep-file retains it)")
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

    try:
        # DEP-05 (optional): timestamped on-device /export backup before any
        # change. A requested backup that fails is fatal — the push must never
        # imply a backup exists when it does not.
        if backup:
            backup_name = f"pre-{time.strftime('%Y%m%d-%H%M%S', time.gmtime())}"
            log(f"REST: writing on-device backup /export file={backup_name}")
            bresp = session.post(
                f"{base}/rest/export",
                json={"file": backup_name},
                timeout=effective_timeout,
                allow_redirects=False,
                stream=True,
            )
            bstatus = bresp.status_code
            bbody = _read_response_capped(bresp, password, user)
            if bstatus not in (200, 201, 204):
                print(
                    redact_secrets(
                        f"error: backup /export failed (status {bstatus}): {bbody[:300]}",
                        password,
                        user,
                    ),
                    file=sys.stderr,
                )
                sys.exit(4)
            log(f"REST: backup written to {backup_name}.rsc on device")

        # Primary path: POST /rest/execute with {"script": payload}. This
        # avoids file handling differences across versions. Redirects are
        # disabled on every call: 3xx fails closed, never followed.
        # stream=True + _read_response_capped bounds the body (an unbounded
        # body read could OOM on a hostile/broken device).
        # The SPKI pin (when set) is verified inside the pinned HTTPS
        # connection, on the same socket as the request and before the
        # Authorization header is written (see _mikrotik_shared).
        log(f"REST: uploading {content_bytes} bytes to {host_for_url} as {user} (direct execute)")
        resp = session.post(
            f"{base}/rest/execute",
            json={"script": payload},
            timeout=effective_timeout,
            allow_redirects=False,
            stream=True,
        )
        if resp.status_code in (200, 201, 204):
            body = _read_response_capped(resp, password, user)
            if verify_this:
                if _REST_SENTINEL in body:
                    log(f"REST: execute OK ({resp.status_code}, verified)")
                    cleaned = "\n".join(
                        line
                        for line in body.splitlines()
                        if _REST_SENTINEL not in line
                    )
                    if cleaned.strip():
                        print(redact_secrets(cleaned, password, user))
                    return
                msg = (
                    f"execute returned {resp.status_code} but the completion sentinel "
                    f"{_REST_SENTINEL!r} was not observed; the device may have applied "
                    "changes — verify device state (use --no-verify-execute on builds "
                    "that do not return /rest/execute output)"
                )
                print(
                    redact_secrets(f"error: {msg} body={body[:500]}", password, user),
                    file=sys.stderr,
                )
                sys.exit(5)
            log(f"REST: execute OK ({resp.status_code}, result not verified)")
            if body and body.strip():
                print(redact_secrets(body, password, user))
            return
        if 300 <= resp.status_code < 400:
            print(f"error: redirect blocked (status {resp.status_code}); refusing to follow", file=sys.stderr)
            sys.exit(4)
        # If execute not allowed, try the file fallback (size-bounded).
        resp_body = _read_response_capped(resp, password, user)
        log(
            f"REST execute returned {resp.status_code}: "
            f"{redact_secrets(resp_body[:500], password, user)}"
        )
        # DEP-02: RouterOS documents file-content editing only up to 60 KB, so
        # the fallback cannot carry a long script; point at SSH instead.
        if content_bytes > _REST_FILE_FALLBACK_MAX_BYTES:
            print(
                f"error: REST execute failed ({resp.status_code}) and content is "
                f"{content_bytes} bytes (> {_REST_FILE_FALLBACK_MAX_BYTES}-byte "
                "file-upload fallback limit); use --method ssh for this payload",
                file=sys.stderr,
            )
            sys.exit(2)
        log("REST: falling back to PUT /rest/file upload — EXPERIMENTAL: RouterOS's file API varies across versions")
        # File upload via /rest/file (PUT) — filename already validated and URL-encoded.
        # RouterOS file API is not well documented; we try PUT with contents field
        put_resp = session.put(
            f"{base}/rest/file/{encoded_filename}",
            json={"contents": content},
            timeout=effective_timeout,
            allow_redirects=False,
            stream=True,
        )
        if put_resp.status_code in (200, 201, 204):
            log(f"REST: file upload OK ({put_resp.status_code}), now importing")
            # RouterOS console accepts single-quoted strings; quoting guards
            # filenames containing spaces/special chars.
            imp = session.post(
                f"{base}/rest/execute",
                json={"script": f"/import file={shlex.quote(filename)}"},
                timeout=effective_timeout,
                allow_redirects=False,
                stream=True,
            )
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
            # DEP-05: best-effort cleanup of the uploaded file (safe default),
            # so stale .rsc copies do not accumulate on the device.
            if not keep_file:
                try:
                    del_resp = session.delete(
                        f"{base}/rest/file/{encoded_filename}",
                        timeout=effective_timeout,
                        allow_redirects=False,
                        stream=True,
                    )
                    del_status = del_resp.status_code
                    del_resp.close()
                    if del_status in (200, 201, 204):
                        log(f"REST: removed remote file {filename} (--keep-file retains it)")
                    else:
                        log(f"warning: could not remove remote file {filename} (status {del_status})")
                except requests.exceptions.RequestException as e:
                    log(
                        f"warning: could not remove remote file {filename}: "
                        f"{redact_secrets(str(e), password, user)}"
                    )
            return
        put_body = _read_response_capped(put_resp, password, user)
        msg = f"error: REST deploy failed: execute={resp.status_code} {resp_body[:1000]} file={put_resp.status_code} {put_body[:1000]}"
        print(redact_secrets(msg, password, user), file=sys.stderr)
        sys.exit(4)
    except requests.exceptions.RequestException as e:
        print(redact_secrets(f"error: REST request failed: {e}", password, user), file=sys.stderr)
        sys.exit(4)


def _wait_for_exit(stdout, deadline: float) -> bool:
    """Poll a paramiko channel until the command exits or ``deadline`` passes.

    ``recv_exit_status()`` blocks forever when a device never terminates the
    command, so callers use this bounded poll first and only then read the
    exit status (which returns immediately once the status is ready).
    """
    while not stdout.channel.exit_status_ready():
        if time.monotonic() >= deadline:
            return False
        time.sleep(0.1)
    return True


def _ssh_host_key_fingerprint(client) -> str | None:
    """Best-effort ``<type> SHA256:<base64>`` of the negotiated host key."""
    try:
        key = client.get_transport().get_remote_server_key()
        digest = hashlib.sha256(key.asbytes()).digest()
        return f"{key.get_name()} SHA256:{base64.b64encode(digest).decode('ascii')}"
    except Exception:
        return None


def deploy_via_ssh(
    host: str,
    user: str,
    password: str,
    port: int,
    content: str,
    filename: str,
    dry_run: bool,
    accept_host_key: bool,
    timeout: int = 60,
    force_destructive: bool = False,
    identity: str = "",
    keep_file: bool = False,
    backup: bool = False,
) -> None:
    # SSRF gate before any SSH dial — enforced even on --dry-run.
    _deny_ssrf_host_or_exit(host)
    host_for_url = format_host_for_url(host)
    # Sanitize filename before SFTP — same gate as REST.
    filename = _sanitize_and_validate_filename(filename)
    # Local destructive pre-scan before any preview or network access.
    # Same verdict on dry-run (exit 2 unless --force-destructive).
    check_destructive_or_exit(content, force_destructive)
    if dry_run:
        auth = f"key={identity}" if identity else "password"
        log(f"DRY-RUN SSH: would sftp {len(content)} bytes to {host_for_url}:{port} as {user} (auth: {auth}) -> /{filename}")
        log(f"DRY-RUN SSH: would ssh {user}@{host_for_url} \"/import file={filename}\"")
        if backup:
            log("DRY-RUN SSH: would first run /export file=pre-<utc-ts> (on-device backup)")
        if not keep_file:
            log("DRY-RUN SSH: would remove the uploaded file after a successful import (--keep-file retains it)")
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

    log(f"SSH: connecting to {host_for_url}:{port} as {user}")
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
        # `--identity` (default off) adds key auth while password stays the
        # default; `look_for_keys`/`allow_agent` remain off so the user's
        # other key material is never offered implicitly.
        # SSH connect timeout stays at a fixed 15s (independent of --timeout,
        # which only bounds the remote /import poll below).
        pinned_sock = open_pinned_socket(addrs, port, 15)
        client.connect(
            hostname=host,
            sock=pinned_sock,
            username=user,
            password=password,
            key_filename=identity or None,
            look_for_keys=False,
            allow_agent=False,
            timeout=15,
        )
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

    # DEP-04: TOFU must be informed, not blind — show the accepted host key.
    if accept_host_key:
        fp = _ssh_host_key_fingerprint(client)
        if fp:
            log(
                f"SSH: host key {fp} accepted via --accept-host-key "
                "(verify this fingerprint against the device before trusting it)"
            )

    try:
        # DEP-05 (optional): timestamped on-device /export backup before any
        # change. A requested backup that fails is fatal.
        if backup:
            backup_name = f"pre-{time.strftime('%Y%m%d-%H%M%S', time.gmtime())}"
            log(f"SSH: writing on-device backup /export file={backup_name}")
            _b_in, b_out, b_err = client.exec_command(
                f"/export file={backup_name}", timeout=15
            )
            if not _wait_for_exit(b_out, time.monotonic() + 30):
                print("error: on-device backup timed out after 30s", file=sys.stderr)
                sys.exit(4)
            b_status = b_out.channel.recv_exit_status()
            if b_status != 0:
                b_msg = b_err.read(MAX_RESPONSE_BYTES + 1).decode(errors="replace")
                print(
                    redact_secrets(
                        f"error: on-device backup failed with exit {b_status}: {b_msg[:300]}",
                        password,
                        user,
                    ),
                    file=sys.stderr,
                )
                sys.exit(4)
            log(f"SSH: backup written to {backup_name}.rsc on device")

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
        # DEP-05: best-effort cleanup of the uploaded file (safe default), so
        # stale .rsc copies do not accumulate on the device.
        if not keep_file:
            try:
                _del_in, del_out, del_err = client.exec_command(
                    f"/file remove {filename}", timeout=15
                )
                if _wait_for_exit(del_out, time.monotonic() + 15):
                    del_status = del_out.channel.recv_exit_status()
                    if del_status == 0:
                        log(f"SSH: removed remote file {filename} (--keep-file retains it)")
                    else:
                        del_msg = del_err.read(MAX_RESPONSE_BYTES + 1).decode(
                            errors="replace"
                        )
                        log(
                            f"warning: could not remove remote file {filename}: "
                            f"{redact_secrets(del_msg[:200], password, user)}"
                        )
                else:
                    log(
                        f"warning: could not remove remote file {filename}: removal timed out"
                    )
            except Exception as e:
                log(
                    f"warning: could not remove remote file {filename}: "
                    f"{redact_secrets(str(e), password, user)}"
                )
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
        help="Force plain HTTP for REST transport (env MIKROTIK_HTTP=1). No legacy SSL=0 fallback here; Rust rsc-ls keeps one behind opt-in RSC_LS_LEGACY_HTTP_SHIM=1",
    )
    p.add_argument(
        "--timeout",
        type=int,
        default=env_int("MIKROTIK_TIMEOUT", 60),
        help="Per-request REST timeout and seconds to wait for the remote SSH /import (env MIKROTIK_TIMEOUT, default 60, clamped 1..300; REST is additionally capped at the device's 60s server-side limit, use --method ssh for long imports; SSH connect stays at fixed 15s)",
    )
    p.add_argument(
        "--accept-host-key",
        action="store_true",
        default=os.getenv("MIKROTIK_ACCEPT_HOST_KEY") == "1",
        help="Trust unknown SSH host keys (trust-on-first-use). WARNING: vulnerable to MITM. The accepted key fingerprint is printed for verification. Env: MIKROTIK_ACCEPT_HOST_KEY=1",
    )
    p.add_argument(
        "--identity",
        default=os.getenv("MIKROTIK_IDENTITY", ""),
        help="Private key file for SSH auth (env MIKROTIK_IDENTITY). Default off: password auth only, and ~/.ssh keys are never offered implicitly",
    )
    p.add_argument(
        "--dry-run",
        action="store_true",
        help="Show what would be done without connecting (preview only: it does NOT validate RouterOS syntax; use the rsc-ls diagnostics / Validate task for that)",
    )
    p.add_argument(
        "--force-destructive",
        action="store_true",
        default=os.getenv("MIKROTIK_FORCE_DESTRUCTIVE") == "1",
        help="Allow content matching the best-effort destructive pre-scan (system reset / reset-configuration hard-blocked, remove requires confirmation). Comments and quoted strings are ignored. Without it the push is refused with exit 2 (env MIKROTIK_FORCE_DESTRUCTIVE=1)",
    )
    p.add_argument(
        "--no-verify-execute",
        action="store_true",
        help="Skip the /rest/execute completion sentinel (RSC_DEPLOY_OK). Only for RouterOS builds that do not return /rest/execute output; without the sentinel a 2xx result is otherwise reported as unverified (exit 5)",
    )
    p.add_argument(
        "--keep-file",
        action="store_true",
        help="Keep the uploaded .rsc on the device after a successful import (default: best-effort remove)",
    )
    p.add_argument(
        "--backup",
        action="store_true",
        help="Write a timestamped /export file=pre-<utc-ts> on the device before pushing; a failed backup aborts the push (exit 4)",
    )
    p.add_argument("--filename", default=None, help="Remote filename (default: basename of file)")
    p.add_argument(
        "--fingerprint",
        default=os.getenv("MIKROTIK_FINGERPRINT"),
        help="SPKI SHA256 pin for REST TLS (env MIKROTIK_FINGERPRINT, format sha256:<hex>). Precedence: CA_FILE => chain+hostname AND pin; pin-only => chain relaxed, pin enforced pre-Auth",
    )
    p.add_argument(
        "--ca-file",
        default=os.getenv("MIKROTIK_CA_FILE"),
        help="Custom CA bundle path for REST TLS (env MIKROTIK_CA_FILE). With a pin: chain+hostname AND pin are both enforced",
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

    # Single-host contract: rsc-ls parses a comma-separated MIKROTIK_HOST and
    # fetches only the primary; the scripts dial exactly one host, so a list
    # is a usage error instead of a confusing DNS failure later.
    if "," in args.host:
        print(
            "error: --host/MIKROTIK_HOST must be a single host; comma-separated "
            "multi-host is only supported by rsc-ls live (which fetches the primary "
            "host only)",
            file=sys.stderr,
        )
        sys.exit(2)

    # SSRF gate before any file handling preview, transport dispatch, or
    # network access — enforced even on --dry-run (no connection attempted).
    _deny_ssrf_host_or_exit(args.host)

    if not args.password and not args.dry_run:
        # Prompt securely if not provided and not dry-run
        try:
            args.password = getpass.getpass(f"Password for {args.user}@{format_host_for_url(args.host)}: ")
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
    # Local destructive pre-scan before any preview or network access.
    # Same verdict on dry-run (exit 2 unless --force-destructive).
    check_destructive_or_exit(content, args.force_destructive)

    method = args.method
    port = args.port
    if port is None:
        if method == "ssh":
            port = 22
        elif method == "rest":
            port = 443
        else:
            # auto: prefer REST, so 443; invalid env falls back with a warning.
            port_raw = os.getenv("MIKROTIK_PORT")
            if port_raw is not None and port_raw.strip():
                try:
                    port = int(port_raw.strip())
                except ValueError:
                    print(
                        f"warning: invalid MIKROTIK_PORT={port_raw!r}, using default 443",
                        file=sys.stderr,
                    )
                    port = 443
            else:
                port = 443

    # Port guard mirrored from the live check: valid TCP ports are 1..65535.
    if not 1 <= port <= 65535:
        print(f"error: invalid port {port}", file=sys.stderr)
        sys.exit(2)

    if args.dry_run:
        log(f"DRY-RUN: {path} -> {format_host_for_url(args.host)} as {args.user} ({len(content)} bytes, method={args.method})")
        # Show first 500 chars
        preview = content[:500].replace("\n", "\\n")
        log(f"Preview: {preview[:200]}...")

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

    # SEC-07: a SPKI pin cannot be enforced over plain HTTP (and Basic
    # credentials travel in cleartext), so never let the flag imply security.
    if method == "rest" and fingerprint is not None and resolve_scheme(port, args.http) == "http":
        print(
            "warning: MIKROTIK_FINGERPRINT is set but the effective scheme is http; "
            "the SPKI pin is not enforced over plain HTTP and credentials are sent in cleartext",
            file=sys.stderr,
        )

    if method == "rest":
        # POST /rest/execute and /import are never retried: only idempotent
        # GETs retry (see the live check), so a repeated import cannot run twice.
        deploy_via_rest(
            args.host,
            args.user,
            args.password or "",
            port,
            ssl_verify,
            content,
            filename,
            args.dry_run,
            force_http=args.http,
            timeout=args.timeout,
            fingerprint=fingerprint,
            ca_file=ca_file,
            force_destructive=args.force_destructive,
            verify_execute=not args.no_verify_execute,
            keep_file=args.keep_file,
            backup=args.backup,
        )
    elif method == "ssh":
        # For SSH, default port 22 if auto gave 443
        if args.port is None and port == 443:
            port = 22
        deploy_via_ssh(
            args.host,
            args.user,
            args.password or "",
            port,
            content,
            filename,
            args.dry_run,
            args.accept_host_key,
            timeout=args.timeout,
            force_destructive=args.force_destructive,
            identity=args.identity or "",
            keep_file=args.keep_file,
            backup=args.backup,
        )
    else:
        print(f"error: unknown method {method}", file=sys.stderr)
        sys.exit(2)

    log("Done.")


if __name__ == "__main__":
    main()
