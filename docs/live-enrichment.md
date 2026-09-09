# Live device enrichment (opt-in)

Disabled by default. When enabled, `rsc-ls` fetches real device data over
the RouterOS REST API into an **in-memory TTL cache** (never touches
`data/commands.toml` or disk) and surfaces it as top-tier completions
(`0!live_…`, detail `live — …`). Completion never blocks: background
hydrator is coalesced, failures are negative-cached, offline falls back
to static placeholders. Sources: `lsp/src/live.rs`, `lsp/src/caps.rs`.

## Enable

```bash
export RSC_LS_LIVE=1                # or MIKROTIK_LIVE=1
export MIKROTIK_HOST="192.168.88.1"
export MIKROTIK_USER="admin"        # default admin
export MIKROTIK_PASS="secret"       # env/keychain ONLY — never settings files
```

Check first: `python scripts/mikrotik-live-check.py --dry-run`, then without
`--dry-run` for a real `GET /rest/interface`. Zed task:
*MikroTik: Live — Check connectivity (opt-in)*.

## Full `MIKROTIK_*` env table

| Var | Default | Notes |
| --- | --- | --- |
| `RSC_LS_LIVE` / `MIKROTIK_LIVE` | off (`0`) | either `=1` enables |
| `MIKROTIK_HOST` | — (required) | IP/hostname; comma-separated multi-host validated, only primary hydrated |
| `MIKROTIK_USER` | `admin` | invalid values fall back with a warning |
| `MIKROTIK_PASS` | — (required) | **never logged**; settings-file values ignored with warning |
| `MIKROTIK_PORT` | `443` | `1..65535` |
| `MIKROTIK_HTTP` | `0` (https) | `=1` forces plain HTTP |
| `MIKROTIK_SSL` | verify on | `=0` disables verification (scheme unchanged; see legacy shim) |
| `MIKROTIK_TIMEOUT` | `5` | per-request seconds, clamped `1..30` |
| `MIKROTIK_FINGERPRINT` | — | `sha256:<64 hex>` SPKI pin; invalid → fail-closed (inactive) |
| `MIKROTIK_CA_FILE` | — | custom CA bundle path (wins over boolean flag) |
| `RSC_LS_LIVE_RESOURCES` | — | JSON array max 8, `{"property","path","field"}` custom resources |
| `RSC_LS_LIVE_ALLOW_LOOPBACK` | off | `=1` allows loopback/private hosts (tests/local only) |

Deploy shares the same `MIKROTIK_*` semantics with different defaults
(timeout 60, SSH support) — see [device-deploy.md](device-deploy.md).

## Settings trust gate

Non-secret fields may live in Zed `settings.json` (`lsp.rsc-ls.binary.env`),
but: **transport/host downgrades from workspace settings are ignored**
unless `RSC_LS_ALLOW_SETTINGS_TRANSPORT=1`; a `pass`/`password` key in
settings is **always ignored with a warning**. Env wins. Never commit
secrets — keep `.zed/settings.json` secret-free (gitignore unless proven).

Legacy shim (off by default): `RSC_LS_LEGACY_HTTP_SHIM=1` restores the old
`MIKROTIK_SSL=0`-on-nonstandard-port → http downgrade. Prefer `MIKROTIK_HTTP=1`.

## TLS: fingerprint / CA

Precedence: custom CA bundle → SPKI pin (`MIKROTIK_FINGERPRINT`) → boolean
`MIKROTIK_SSL`. A pin or CA counts as verification even with `MIKROTIK_SSL=0`.
TLS 1.2/1.3. Malformed pin fails closed (live inactive, logged).

## SSRF deny

Host validation + SSRF denial run before any request (`169.254.169.254`
denied; loopback/private denied without the allow-loopback flag).
Shared with deploy/live-check via `scripts/_mikrotik_shared.py`.

## Caps

| Cap | Value |
| --- | --- |
| `MAX_LIVE_RESPONSE_BYTES` | 512 KiB per response |
| `MAX_LIVE_ITEMS` | 500 values per cache entry |
| `MAX_LIVE_VALUE_LEN` | 64 chars per value |
| `MAX_CACHE_ENTRIES` | 16 collections |
| `LIVE_TTL_SECS` | 60 s positive TTL |
| `LIVE_NEGATIVE_TTL_SECS` | 15 s negative TTL |
| coalesce window / max blocking | 2 s (`LIVE_FETCH_BLOCKING_TIMEOUT_SECS`) |
| `LIVE_CUSTOM_RESOURCES_MAX` | 8 custom resources |
| `LIVE_MAX_HOSTS` | 4 hosts (primary hydrated) |
| fetch-thread semaphore | 2 (`live.rs`) |

Invalidation: `didChange` never clears; `didClose`, `LiveConfig` change,
and `rsc.live.refresh` do (`rsc.live.status` is read-only).

## live-check exit codes

`scripts/mikrotik-live-check.py`: **0** Live OK (`Live OK: N interfaces`),
**2** usage error (missing host), **4** live FAIL (network/auth/status/parse).
Supports `--dry-run` and `--json`; never logs the password.
