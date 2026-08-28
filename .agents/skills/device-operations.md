# Skill: Device Operations (Deploy + Live)

## When to Use

Trigger this skill when the task involves any of: `mikrotik-deploy`, `mikrotik-live-check`, `MIKROTIK_HOST` / `MIKROTIK_USER` / `MIKROTIK_PASS`, `RSC_LS_LIVE` / `MIKROTIK_LIVE`, `live enrichment`, `LiveCache`, `device REST` / `SSH`, `tasks.json`, `manual device testing`, `/rest/interface`, or pushing `.rsc` files to a RouterOS device.

For LSP internals (completion hydrator, cache coalescing) see `language-server`; for day-to-day make targets see `development-workflow`.

## Overview

Two companion scripts provide all device I/O. Both **never log `MIKROTIK_PASS`** (not to stdout, stderr, or tasks).

* Conceptually both share a REST + SSH duality, but in practice:
  * `scripts/mikrotik-deploy.py` supports **REST and SSH** (auto-select).
  * `scripts/mikrotik-live-check.py` is **REST only** — it validates the exact path `rsc-ls` uses for Live enrichment (`GET /rest/interface`).
* Both resolve scheme as `https` by default; `MIKROTIK_HTTP=1` / `--http` forces `http`. `MIKROTIK_SSL=0` / `--no-ssl-verify` only disables certificate verification (rustls `ServerCertVerifier` in `rsc-ls`), never the scheme — legacy shim on non-standard ports warns.
* Canonical caps live in `lsp/src/caps.rs` — never duplicate values here; look them up.

## mikrotik-deploy.py — Push .rsc to Device

**Transports:** REST via `requests` (`POST /rest/execute`, fallback `PUT /rest/file` + `/import`) + SSH via `paramiko` (SFTP + `/import`). Auto-select prefers REST when `requests` is installed; override with `MIKROTIK_METHOD` / `--method {auto,rest,ssh}`.

**Env vars (flags override env):**

| Var | Default | Notes |
|-----|---------|-------|
| `MIKROTIK_HOST` | — (required) | IP/hostname; validated before connect |
| `MIKROTIK_USER` | `admin` | — |
| `MIKROTIK_PASS` | — (required unless `--dry-run`) | Prompted via `getpass` if missing; never logged |
| `MIKROTIK_PORT` | `443` rest / `22` ssh | Auto-resolved per method |
| `MIKROTIK_SSL` | verify | `0` → disable TLS verification |
| `MIKROTIK_HTTP` | `https` | `1` → force plain HTTP |
| `MIKROTIK_TIMEOUT` | `60` | SSH `/import` wait (clamped `>=1`); Live uses `5` |
| `MIKROTIK_ACCEPT_HOST_KEY` | reject | `1` → TOFU `AutoAddPolicy` (MITM risk, warn) |
| `MIKROTIK_METHOD` | `auto` | `rest` / `ssh` / `auto` |

**Safety:** always `--dry-run` first. Dry-run prints `shlex.quote`'d commands (`/import file='<name>'`), byte count, preview (first 500 chars, `\n` escaped), and never requires `MIKROTIK_PASS` or deps. Enforces `5 MiB` file cap, rejects empty files, scans `/import` output for failure markers (`syntax error`, `input does not match`, `bad command name`, `failure:`) — direct `/rest/execute` output is printed verbatim without scanning.

**CLI examples:**

```bash
python scripts/mikrotik-deploy.py path/to/file.rsc --dry-run
python scripts/mikrotik-deploy.py path/to/file.rsc --host 192.168.88.1 --user admin --dry-run
MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret python scripts/mikrotik-deploy.py file.rsc --method rest
MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret python scripts/mikrotik-deploy.py file.rsc --method ssh --accept-host-key
python scripts/mikrotik-deploy.py file.rsc --http --port 80 --dry-run
```

## mikrotik-live-check.py — Verify Live REST

Mirrors `lsp/src/live.rs: LiveConfig::from_env` so the same env works for `rsc-ls`.

**What it does:** authenticated `GET /rest/interface` via Basic Auth (`requests` preferred, `urllib` fallback), reports item count, caps response at `MAX_LIVE_RESPONSE_BYTES` 512 KiB (`lsp/src/caps.rs`). Never logs `MIKROTIK_PASS` (redacts if it appears in errors).

**Flags:** `--host` / `--user` / `--port` / `--no-ssl-verify` / `--http` / `--timeout` / `--json` / `--dry-run` + compat `--method` (ignored, kept for `tasks.json`). Env: `MIKROTIK_HOST` / `USER` / `PASS` / `PORT` / `SSL` / `HTTP` / `TIMEOUT`.

**Defaults & validation:** `PORT` 443, `TIMEOUT` 5s clamped `1..30s` (like `LIVE_TIMEOUT_SECS`), host validation (`validate_host`: non-empty, `<=253`, no null/control, no `@?#%` space, no `/\`), port `1..65535`. SSRF deny `169.254.169.254` is enforced in `rsc-ls` (`is_ssrf_denied_host`); the check surfaces auth/network failures the same way. `fe80::` literals are bracket-wrapped via `format_host_for_url`.

**Exit codes:** `0` OK (reachable, valid JSON list), `2` usage (missing host/pass), `4` live fail (network, auth, non-200, parse, host/port validation, too-large response).

**CLI examples:**

```bash
python scripts/mikrotik-live-check.py --dry-run
python scripts/mikrotik-live-check.py --dry-run --json
MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret python scripts/mikrotik-live-check.py
MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret python scripts/mikrotik-live-check.py --json
python scripts/mikrotik-live-check.py --host 192.168.88.1 --user admin --no-ssl-verify --timeout 10
```

## Live Enrichment (rsc-ls, Opt-In)

Disabled by default. Enable with `RSC_LS_LIVE=1` or `MIKROTIK_LIVE=1` + `MIKROTIK_HOST` / `MIKROTIK_PASS` (env or keychain). In-memory TTL cache only — never persists, never touches `data/commands.toml` (separate pipelines per `AGENTS.md` Hard rule #6).

**Caps — source `lsp/src/caps.rs`:**

| Cap | Value | Meaning |
|-----|-------|---------|
| `LIVE_TTL_SECS` | 60s | Fresh cache TTL |
| `LIVE_TIMEOUT_SECS` | 5s (clamped 1..30s via `MIKROTIK_TIMEOUT`) | Per-request timeout |
| `LIVE_FETCH_BLOCKING_TIMEOUT_SECS` | 2s | Max blocking time completion waits; background hydrator non-blocking |
| `LIVE_NEGATIVE_TTL_SECS` | 15s | Negative cache after failed fetch (retry gate) |
| `LIVE_MAX_HOSTS` | 4 | Cap on comma-separated `MIKROTIK_HOST` (primary hydrates) |
| `LIVE_CUSTOM_RESOURCES_MAX` | 8 | Cap on `RSC_LS_LIVE_RESOURCES` JSON array |
| `MAX_LIVE_ITEMS` / `MAX_LIVE_VALUE_LEN` / `MAX_LIVE_RESPONSE_BYTES` / `MAX_CACHE_ENTRIES` | 500 / 64 / 512 KiB / 16 | Response and cache bounds |

**Behavior:** `get_cached_or_fetch_background` serves fresh hits; miss/stale triggers `trigger_background_fetch` (coalesced within 2s) → `fetch_resource` on thread. URL via `url` crate + `build_rest_url` (validates host, rejects SSRF, brackets bare `fe80::` via `format_host_for_url`). TLS: `build_insecure_agent` installs rustls `ServerCertVerifier` (`NoCertificateVerification`) when `MIKROTIK_SSL=0`, else default verifier. `ureq::Agent` cached by `(timeout, ssl_verify)`.

**Custom resources:** `RSC_LS_LIVE_RESOURCES` JSON array (max 8), each `{"property","path","field"}` — `path` must start `/rest`, `property`/`field` `<=64` chars. Example: `[{"property":"my-prop","path":"/rest/interface","field":"name"}]` augments `property=` value completions.

**Hot-reload:** `workspace/didChangeConfiguration` merges `rsc.live` / `mikrotik` / `MIKROTIK_*` via `LiveConfig::apply_settings_value`; `workspace/executeCommand` `rsc.live.refresh` / `rsc.live.status` for cache control.

## Zed Tasks (languages/rsc/tasks.json)

Template → activation:

```bash
cp languages/rsc/tasks.json .zed/tasks.json
```

Six tasks (all `cwd: $ZED_WORKTREE_ROOT`):

| Label | Command | Notes |
|-------|---------|-------|
| `MikroTik: Deploy current file (REST)` | `python3 scripts/mikrotik-deploy.py $ZED_FILE --method rest` | `use_new_terminal:true`, `reveal:always` |
| `MikroTik: Deploy current file (SSH)` | `... --method ssh` | — |
| `MikroTik: Dry-run deploy (preview)` | `... --dry-run` | `allow_concurrent_runs:true` |
| `MikroTik: Validate RSC syntax` | `... --dry-run` with `MIKROTIK_HOST=dry-run-only` | No device needed |
| `MikroTik: Live — Check connectivity (opt-in)` | `python3 scripts/mikrotik-live-check.py --host ${input:mikrotik_host} --user ${input:mikrotik_user} --method rest` | Prompts for host/user |
| `MikroTik: Live — Enable enrichment (set RSC_LS_LIVE=1)` | `echo` hint | Never stores pass in `tasks.json` — use env/keychain |

Run via Zed `task: spawn`. All deploy tasks require `MIKROTIK_HOST/USER/PASS` in env/keychain; the `echo` task documents `RSC_LS_LIVE=1`.

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `timeout` / `Live FAIL: network error` | Device unreachable or `TIMEOUT` too low | `mikrotik-live-check.py --dry-run` to preview URL; raise `--timeout` (1..30s clamp); `RSC_LS_LOG=debug zed --foreground` |
| `SSRF deny` / `invalid host` | Host is `169.254.169.254` (link-local, blocked) or contains `@?#% /` | Use real device IP/hostname; `validate_host` rejects URI delimiters — check `lsp/src/live.rs: is_ssrf_denied_host` |
| `host validation failed` / `exceeds 253 chars` | Bad `MIKROTIK_HOST` (empty, control chars, slash) | Trim, remove scheme/path, keep bare host/IP only |
| `TLS mismatch` / `certificate verify failed` | `MIKROTIK_SSL=0` not set for self-signed, or `--http` missing for plain HTTP | `MIKROTIK_SSL=0` for insecure TLS (rustls) or `MIKROTIK_HTTP=1` / `--http` for `http://`; `--no-ssl-verify` never selects scheme |
| `missing MIKROTIK_PASS` (exit 2) | Pass not in env and no TTY prompt | `export MIKROTIK_PASS=...` or keychain; `--dry-run` needs no pass; never put pass in `tasks.json` or logs |
| `response too large` | Device returned `>512 KiB` | Check device REST config; `MAX_LIVE_RESPONSE_BYTES` in `caps.rs` protects OOM — filter on device side |
| `auth failed` / `http status 401/403` | Wrong `USER`/`PASS` or REST disabled | Verify credentials; REST enabled by default — check `/ip/service` |
| `paramiko` / `requests` missing | Optional deps not installed | `pip install requests paramiko`; dry-run works without them |

## Verification

```bash
# Preview without touching a device (no pass, no network)
python scripts/mikrotik-deploy.py path/to/file.rsc --dry-run
python scripts/mikrotik-live-check.py --dry-run --json

# Real check — exit 0 OK, 2 usage, 4 live fail
MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret python scripts/mikrotik-live-check.py --json

# Live enrichment end-to-end (restart Zed so env propagates)
RSC_LS_LIVE=1 MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret zed --foreground
# then in Zed: open .rsc → trigger interface/address completion → zed: open log → [rsc-ls]
```

Check `lsp/src/caps.rs` for authoritative caps; never hardcode RouterOS version — see `data/commands.toml` header.

## Related Skills

* `language-server` — `lsp/src/live.rs` + `caps.rs` internals (stale-while-revalidate, coalescing, `build_rest_url`, `url` crate, rustls).
* `development-workflow` — `make` targets, `make validate`, `zed: open log`, `RSC_LS_LOG`, PATH vs auto-download.
* `zed-extension-dev` — publishing, `extension.toml` `rev`, WASM shim.
