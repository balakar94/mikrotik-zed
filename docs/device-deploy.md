# Device deploy (optional, local only)

Push a validated `.rsc` file to a real router and `/import` it — from a Zed
task or `scripts/mikrotik-deploy.py`. Never logs `MIKROTIK_PASS`.
Success caveat: HTTP 200 / SSH exit 0 does **not** prove import success,
so output is scanned for failure markers (`syntax error`, `bad command
name`, `failure:` …) and surfaced as real failures.

## Transports

- **REST** — RouterOS REST API; script execution with file-upload +
  `/import` fallback for long scripts.
- **SSH** — SFTP upload + `/import` over an interactive session
  (connect timeout fixed 15 s).

## Env

Required: `MIKROTIK_HOST`, `MIKROTIK_PASS` (`MIKROTIK_USER` defaults `admin`).
Optional: `MIKROTIK_PORT` (auto 443 REST / 22 SSH), `MIKROTIK_METHOD`
(`rest`/`ssh`/auto), `MIKROTIK_HTTP=1` (plain HTTP), `MIKROTIK_SSL=0`
(no verify), `MIKROTIK_TIMEOUT` (default 60, clamped 1..300),
`MIKROTIK_ACCEPT_HOST_KEY=1` (SSH TOFU, MITM warning),
`MIKROTIK_FINGERPRINT` (`sha256:<hex>`), `MIKROTIK_CA_FILE`.
Prefer env/getpass for the password — argv is visible in process listings.
Same `MIKROTIK_*` semantics as live (which is REST-only, timeout default 5).

## Dry-run first (always)

```bash
python scripts/mikrotik-deploy.py demo.rsc --dry-run
MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret \
  python scripts/mikrotik-deploy.py demo.rsc --method rest
```

## The 6 Zed tasks (`languages/rsc/tasks.json` → copy to `.zed/tasks.json`)

| Task label | What it does |
| --- | --- |
| MikroTik: Check script (dry-run, no device) | deploy preview, no network |
| MikroTik: Deploy current file (REST) | `$ZED_FILE` via REST |
| MikroTik: Deploy current file (SSH) | `$ZED_FILE` via SSH |
| MikroTik: Validate RSC syntax (local only, no device) | size/UTF-8 sanity (≤5 MiB); semantics come from `rsc-ls` |
| MikroTik: Live — Check connectivity (opt-in) | prompts host/user, pass from env/keychain |
| MikroTik: Live — Enable enrichment (set RSC_LS_LIVE=1) | prints env setup hint, changes nothing |

See `python scripts/mikrotik-deploy.py --help` and the
[device-operations skill](../.agents/skills/device-operations.md).
Live health: [live-enrichment.md](live-enrichment.md).
