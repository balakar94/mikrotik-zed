# Device deploy (optional, local only)

Push a validated `.rsc` file to a real router over REST or SSH — from a Zed
task or `scripts/mikrotik-deploy.py`. Never logs `MIKROTIK_PASS`.

Device-side prerequisites (`www-ssl`, certificate, `rest-api` policy):
[index.md#prerequisites](index.md#prerequisites).

## Prerequisites

- Python 3 (`python3` on macOS/Linux; on Windows use the launcher you
  installed — `python3` is not guaranteed).
- REST transport: `requests`. SSH transport: `paramiko`. Dry-run works
  without either.
- A file in Zed saved on disk (`$ZED_FILE` is injected by the task).

## Transports

- **REST** — `POST /rest/execute` with a completion sentinel (below);
  size-bounded `/rest/file` upload + `/import` fallback for short scripts
  (see [verification](#verification)).
- **SSH** — SFTP upload + `/import` over an interactive session
  (connect timeout fixed at 15 s).

## Env

Required: `MIKROTIK_HOST` (exactly one host), `MIKROTIK_PASS`
(`MIKROTIK_USER` defaults to `admin`).

| Variable | Default | Notes |
| --- | --- | --- |
| `MIKROTIK_HOST` | — (required) | Single host; a comma list is rejected (only `rsc-ls` live parses lists, primary only) |
| `MIKROTIK_USER` | `admin` | Invalid values fall back with a warning |
| `MIKROTIK_PASS` | — (required unless dry-run) | Environment or `getpass` prompt; `--pass` is visible in process listings |
| `MIKROTIK_PORT` | auto | `443` REST / `22` SSH |
| `MIKROTIK_METHOD` | `auto` | `rest`, `ssh`, or auto (prefers REST when `requests` is installed) |
| `MIKROTIK_HTTP` | https | `1` forces plain HTTP (cleartext credentials) |
| `MIKROTIK_SSL` | verify | `0` disables TLS verification; never changes the scheme |
| `MIKROTIK_TIMEOUT` | `60` | REST request and SSH `/import` wait, clamped `1..300`; REST is additionally capped server-side at 60 s (use SSH for long imports) |
| `MIKROTIK_ACCEPT_HOST_KEY` | reject | `1` trusts an unknown SSH host key (TOFU); the accepted fingerprint is printed |
| `MIKROTIK_IDENTITY` | — | Private key file for SSH auth; no implicit `~/.ssh` keys are offered |
| `MIKROTIK_FINGERPRINT` | — | REST SPKI pin `sha256:<hex>`; malformed fails closed |
| `MIKROTIK_CA_FILE` | — | Custom CA bundle for REST |
| `MIKROTIK_FORCE_DESTRUCTIVE` | off | `1` acknowledges the destructive pre-scan |

## Flags

| Flag | Effect |
| --- | --- |
| `--dry-run` | Preview only: no connection, no password, no transport dependency. Still requires `MIKROTIK_HOST` |
| `--method {auto,rest,ssh}` | Transport selection |
| `--backup` | Write `/export file=pre-<utc-ts>` on the device first; a failed backup aborts with exit 4 |
| `--keep-file` | Keep the uploaded `.rsc` on the device (default: best-effort remove) |
| `--no-verify-execute` | Skip the REST completion sentinel (only for builds that return no `/rest/execute` output) |
| `--identity <path>` | SSH private key; password stays the default |
| `--accept-host-key` | SSH TOFU; fingerprint is printed for verification |
| `--force-destructive` | Acknowledge the destructive pre-scan (after a backup) |
| `--timeout <s>` | Clamp overrides |
| `--fingerprint` / `--ca-file` | REST TLS pin / CA bundle |

Run `python scripts/mikrotik-deploy.py --help` for the full list.

## Dry-run first (always)

Dry-run validates the local file, host, filename, and destructive content,
prints a preview, and does not connect. It **requires a host** but no
password and no transport dependency:

```bash
MIKROTIK_HOST=192.168.88.1 \
  python scripts/mikrotik-deploy.py demo.rsc --dry-run
```

Real push (password from the environment or the interactive prompt):

```bash
MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret \
  python scripts/mikrotik-deploy.py demo.rsc --method rest --backup
```

## Verification

- **HTTP 200 / SSH exit 0 does not prove success.** `/import` output is
  scanned for high-confidence failure markers (`syntax error`,
  `input does not match`, `bad command name`, `failure:`) and treated as
  failed (exit 5). Direct `/rest/execute` output is not scanned for markers —
  it is verified with the sentinel instead.
- **REST sentinel:** the executed payload gets a final
  `:put "RSC_DEPLOY_OK"` line. Because RouterOS stops a script on the first
  error, seeing the sentinel proves the last statement was reached. Its
  absence is reported as *unverified* (exit 5) with an explicit "device may
  have applied changes" message. A payload ending in a line continuation
  cannot carry the sentinel; the push is logged as unverified.
  `--no-verify-execute` opts out for builds that return no execute output.
- **REST file fallback** is limited to 60 KiB of content; larger scripts must
  use `--method ssh` (exit 2 with that hint).
- **SSH TOFU:** with `--accept-host-key`, the negotiated host-key SHA256
  fingerprint is printed — verify it against the device before trusting it.

## Destructive content

A best-effort local pre-scan runs before any connection (including
dry-run), ignoring comments and quoted strings:

- `system reset` / `reset-configuration` — hard-blocked.
- `remove` — requires explicit confirmation.

Without `--force-destructive` the push is refused with exit 2, the offending
line numbers, and an `/export` backup hint. The gate is a safety net, not a
sandbox: dynamically constructed commands are not detected. Prefer
`disable` over `remove` while testing (see [recipes](recipes.md)).

## The 6 Zed tasks

Template (`languages/rsc/tasks.json`) → activation: copy to
`.zed/tasks.json` **and** make the `scripts/` path reachable from the
worktree (`cwd` is `$ZED_WORKTREE_ROOT`). The tasks work out of the box when
the worktree is this repository; elsewhere, point them at an installed copy
of the scripts or use absolute paths.

| Task label | What it does | Needs |
| --- | --- | --- |
| *MikroTik: Validate file readability (local only, no device)* | File exists, non-empty, ≤ 5 MiB, valid UTF-8; semantic checks come from `rsc-ls` diagnostics. Saves the current file first | — |
| *MikroTik: Check script (dry-run, no device)* | Deploy preview, no connection. Saves the current file first | `MIKROTIK_HOST` |
| *MikroTik: Live — Check connectivity (opt-in)* | Prompts host/user/timeout; password from env or the terminal prompt | Python 3 |
| *MikroTik: Live — Check connectivity --dry-run (opt-in)* | URL/scheme preview, no connection | — |
| *MikroTik: Deploy current file (REST)* | `$ZED_FILE` via REST. Saves the current file first | `MIKROTIK_HOST`, `MIKROTIK_PASS`, `requests` |
| *MikroTik: Deploy current file (SSH)* | `$ZED_FILE` via SSH | `MIKROTIK_HOST`, `MIKROTIK_PASS`, `paramiko` |

All deploy/live tasks keep secrets out of `tasks.json`. Workflow order:
Validate → Check → (backup) → Deploy REST/SSH → verify on device. Live
enrichment setup: [live-enrichment.md](live-enrichment.md).

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success (sentinel or import markers clean) |
| 2 | Usage/validation: missing or invalid host/port/filename, empty file, comma host list, destructive refusal, REST fallback too large |
| 3 | Missing transport dependency (`requests` / `paramiko`) |
| 4 | Network/auth/TLS/HTTP failure; redirect blocked; failed `--backup` |
| 5 | Import or execute failed, or execute result unverified (sentinel absent) |

Live-check has its own ladder (0/2/4): [live-enrichment.md](live-enrichment.md#live-check-exit-codes).
