# Skill: Device Operations (Deploy + Live)

## When to Use

Trigger this skill when the task involves any of: `mikrotik-deploy`, `mikrotik-live-check`, `MIKROTIK_HOST` / `MIKROTIK_USER` / `MIKROTIK_PASS`, `RSC_LS_LIVE` / `MIKROTIK_LIVE`, `live enrichment`, `LiveCache`, `device REST` / `SSH`, `tasks.json`, `manual device testing`, `/rest/interface`, or pushing `.rsc` files to a RouterOS device.

For LSP internals (completion hydrator, cache coalescing) see `language-server`; for day-to-day make targets see `development-workflow`. User-facing reference: `docs/configuration.md`, `docs/live-enrichment.md`, `docs/device-deploy.md`.

## Overview

Two companion scripts provide all device I/O. Both **never log `MIKROTIK_PASS`** (not to stdout, stderr, or tasks).

* Conceptually both share a REST + SSH duality, but in practice:
  * `scripts/mikrotik-deploy.py` supports **REST and SSH** (auto-select).
  * `scripts/mikrotik-live-check.py` is **REST only** — it validates the exact path `rsc-ls` uses for Live enrichment (`GET /rest/interface`).
* Both resolve scheme as `https` by default; `MIKROTIK_HTTP=1` / `--http` forces `http`. `MIKROTIK_SSL=0` / `--no-ssl-verify` only disables certificate verification (rustls `ServerCertVerifier` in `rsc-ls`), never the scheme — the legacy `port 80 + SSL=0 → http` shim is OFF by default (opt back in with `RSC_LS_LEGACY_HTTP_SHIM=1`); prefer `MIKROTIK_HTTP=1`.
* Both scripts **reject a comma-separated `MIKROTIK_HOST`** (single-host contract, exit 2). `rsc-ls` live still parses a comma list, caps it at `LIVE_MAX_HOSTS`, and fetches the **primary only**.
* Canonical caps live in `lsp/src/caps.rs` — never duplicate values here; look them up.

## mikrotik-deploy.py — Push .rsc to Device

**Transports:** REST via `requests` (`POST /rest/execute` + `RSC_DEPLOY_OK` sentinel; size-bounded `PUT /rest/file` + `/import` fallback ≤ 60 KiB) + SSH via `paramiko` (SFTP + `/import`). Auto-select prefers REST when `requests` is installed; override with `MIKROTIK_METHOD` / `--method {auto,rest,ssh}`.

**Env vars (flags override env):**

| Var | Default | Notes |
|-----|---------|-------|
| `MIKROTIK_HOST` | — (required) | IP/hostname; **one host**; validated before connect |
| `MIKROTIK_USER` | `admin` | — |
| `MIKROTIK_PASS` | — (required unless `--dry-run`) | Prompted via `getpass` if missing; never logged |
| `MIKROTIK_PORT` | `443` rest / `22` ssh | Auto-resolved per method |
| `MIKROTIK_SSL` | verify | `0` → disable TLS verification |
| `MIKROTIK_HTTP` | `https` | `1` → force plain HTTP |
| `MIKROTIK_TIMEOUT` | `60` | REST request + SSH `/import` wait, clamped `1..300`; REST additionally capped at the device's 60 s server-side limit; SSH connect fixed at 15 s |
| `MIKROTIK_ACCEPT_HOST_KEY` | reject | `1` → TOFU `AutoAddPolicy`; the accepted host-key SHA256 fingerprint is printed (verify it) |
| `MIKROTIK_IDENTITY` | — | Private key path for SSH auth; `look_for_keys`/`allow_agent` stay off |
| `MIKROTIK_FINGERPRINT` / `MIKROTIK_CA_FILE` | — | REST SPKI pin / CA bundle |
| `MIKROTIK_FORCE_DESTRUCTIVE` | off | `1` acknowledges the destructive pre-scan |
| `MIKROTIK_METHOD` | `auto` | `rest` / `ssh` / `auto` |

**Verification:** HTTP 200 / SSH exit 0 alone does not prove success. `/import` output is scanned for `syntax error`, `input does not match`, `bad command name`, `failure:` (exit 5 on a match). Direct `/rest/execute` gets a trailing `:put "RSC_DEPLOY_OK"`; absence of the sentinel is reported as unverified (exit 5, "device may have applied changes") unless `--no-verify-execute` is passed. Payloads ending in a line continuation run unverified (logged). Full exit ladder: **2** usage/validation/destructive refusal/fallback-too-large, **3** missing transport dependency, **4** network/auth/TLS/failed `--backup`, **5** import/execute failed or unverified.

**Safety:** always `--dry-run` first — it validates host/file/filename/destructive content and previews without connecting, and still **requires `MIKROTIK_HOST`** (no password, no deps). Enforce `5 MiB` file cap, reject empty files. Destructive pre-scan ignores comments and quoted strings; `system reset` / `reset-configuration` are hard-blocked; `remove` requires explicit confirmation; failure prints line numbers plus `/export` hint. `--backup` writes `pre-<utc-ts>` on-device first and aborts (exit 4) if the backup fails; `--keep-file` retains the uploaded script (default: best-effort remove).

**CLI examples:**

```bash
MIKROTIK_HOST=192.168.88.1 python scripts/mikrotik-deploy.py file.rsc --dry-run
MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret python scripts/mikrotik-deploy.py file.rsc --method rest --backup
MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret python scripts/mikrotik-deploy.py file.rsc --method ssh --accept-host-key
MIKROTIK_HOST=192.168.88.1 python scripts/mikrotik-deploy.py file.rsc --http --port 80 --dry-run
```

## mikrotik-live-check.py — Verify Live REST

Mirrors `lsp/src/live_config.rs: LiveConfig::from_env` so the same env works for `rsc-ls`.

**What it does:** authenticated `GET /rest/interface` via Basic Auth (`requests` preferred, `urllib` fallback), reports item count, caps the response at `MAX_LIVE_RESPONSE_BYTES`. Never logs `MIKROTIK_PASS` (redacts if it appears in errors). Rejects a comma-separated host as a usage error; warns when the host is loopback/private and `RSC_LS_LIVE_ALLOW_LOOPBACK=1` is not set (exit 0 does not imply LSP activation).

**Flags:** `--host` / `--user` / `--port` / `--no-ssl-verify` / `--http` / `--timeout` / `--json` / `--dry-run` / `--fingerprint` / `--ca-file` + compat `--method` (ignored, kept for `tasks.json`). Env: `MIKROTIK_HOST` / `USER` / `PASS` / `PORT` / `SSL` / `HTTP` / `TIMEOUT` / `FINGERPRINT` / `CA_FILE`.

**Defaults & validation:** `PORT` 443, `TIMEOUT` 5 s clamped `1..30`, host validation (non-empty, `<=253`, no null/control, no `@?#%` space, no `/\`), port `1..65535`. `fe80::` literals are bracket-wrapped via `format_host_for_url`.

**Exit codes:** `0` OK (reachable, valid JSON list), `2` usage (missing host/pass, comma host list, invalid params), `4` live fail (network, auth, non-200, parse, host/port validation, too-large response).

**CLI examples:**

```bash
python scripts/mikrotik-live-check.py --dry-run --host 192.168.88.1
MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret python scripts/mikrotik-live-check.py
python scripts/mikrotik-live-check.py --host 192.168.88.1 --user admin --no-ssl-verify --timeout 10
```

## Live Enrichment (rsc-ls, Opt-In)

Disabled by default. Enable with `RSC_LS_LIVE=1` (or `MIKROTIK_LIVE=1`) plus `MIKROTIK_HOST` / `MIKROTIK_PASS`. Transport env must be set where the server process sees it: `lsp.rsc-ls.binary.env` is the reliable Zed surface; terminal exports do not reach a GUI-launched Zed. `lsp.rsc-ls.settings` is **not delivered** (the shim implements no workspace-configuration hook) — do not document it as a live config path. In-memory TTL cache only — never persists, never touches `data/commands.toml` (separate pipelines per `AGENTS.md` Hard rule #6).

**LAN targets:** loopback/RFC 1918/ULA hosts require `RSC_LS_LIVE_ALLOW_LOOPBACK=1`; otherwise `is_active()` is false with reason `host denied by live policy (SSRF/loopback/operator deny)`. Operator deny prefixes (`RSC_LS_LIVE_DENY_PREFIXES`) are checked first and never relaxed by the flag.

**Inactive reasons (single source `LiveConfig::inactive_reason`):** invalid fingerprint (fail-closed); opt-in not set; missing host; missing pass; host denied by policy; invalid port. `log_status()` logs `live enabled but inactive — <reason>`, and the same string rides `rsc.live.status` and the initialize log.

**Caps — source `lsp/src/caps.rs`:**

| Cap | Value | Meaning |
|-----|-------|---------|
| `LIVE_TTL_SECS` | 60s | Fresh cache TTL |
| `LIVE_TIMEOUT_SECS` | 5s (clamped 1..30s via `MIKROTIK_TIMEOUT`) | Per-request timeout |
| `LIVE_FETCH_BLOCKING_TIMEOUT_SECS` | 2s | Max blocking time completion waits; background hydrator non-blocking |
| `LIVE_NEGATIVE_TTL_SECS` | 15s | Negative cache after failed fetch (retry gate) |
| `LIVE_MAX_HOSTS` | 4 | Cap on comma-separated `MIKROTIK_HOST` (primary hydrates) |
| `LIVE_CUSTOM_RESOURCES_MAX` | 8 | Cap on `RSC_LS_LIVE_RESOURCES` JSON array |
| `MAX_LIVE_DENY_PREFIXES` / `MAX_LIVE_DENY_PREFIXES_BYTES` | 32 / 2 KiB | Caps on `RSC_LS_LIVE_DENY_PREFIXES` operator SSRF deny list |
| `MAX_LIVE_ITEMS` / `MAX_LIVE_VALUE_LEN` / `MAX_LIVE_RESPONSE_BYTES` / `MAX_CACHE_ENTRIES` | 500 / 64 / 512 KiB / 16 | Response and cache bounds |

**Resource coverage:** `GET` requests carry `?.proplist=<field>` so only the needed field is fetched. Interfaces/bridges/ports from `/rest/interface`; `in-interface-list`/`out-interface-list` (and `list` under `/interface/list`) from `/rest/interface/list`; addresses, firewall address-lists, chains, and pools from the IPv4/IPv6 table matching the menu family (IPv6 has no mangle table).

**Behavior:** `get_cached_or_fetch_background` serves fresh hits; miss/stale triggers `trigger_background_fetch` (coalesced within 2s) → `fetch_resource` on a thread. URL via `url` crate + `build_rest_url` (validates host, rejects SSRF, brackets bare `fe80::`). Expanded/zero-padded IPv6 literals are accepted and normalized; non-canonical IPv4 spellings (decimal/hex/octal/short) are refused. TLS: `MIKROTIK_SSL=0` installs a rustls `NoCertificateVerification` verifier, else the default; pin/CA override.

**Custom resources:** `RSC_LS_LIVE_RESOURCES` JSON array (max 8), each `{"property","path","field"}` — `path` must start `/rest`, `property`/`field` `<=64` chars. Example: `[{"property":"my-prop","path":"/rest/interface","field":"name"}]` augments `property=` value completions.

**Cache control:** the server advertises `rsc.live.refresh` / `rsc.live.status` via `executeCommandProvider`, but **no Zed surface invokes them today** — status (including `inactive_reason`) is only visible in the log. Invalidation: `didClose`, connection-identity change, TTL; `didChange` never clears.

## Zed Tasks (languages/rsc/tasks.json)

Template → activation:

```bash
cp languages/rsc/tasks.json .zed/tasks.json
```

Six tasks (all `cwd: $ZED_WORKTREE_ROOT`; the `scripts/` paths only resolve in this repo — elsewhere use absolute paths or an installed copy):

| Label | Command | Notes |
|-------|---------|-------|
| `MikroTik: Validate file readability (local only, no device)` | inline Python readability preflight on `$ZED_FILE` | `reveal:no_focus`, `hide:on_success`, `save:current` |
| `MikroTik: Check script (dry-run, no device)` | `python3 scripts/mikrotik-deploy.py $ZED_FILE --dry-run` | Requires `MIKROTIK_HOST`; no password, no network, `save:current` |
| `MikroTik: Live — Check connectivity (opt-in)` | `python3 scripts/mikrotik-live-check.py --host ${input:…} --user … --timeout …` | Prompts host/user/timeout; pass from env or prompt |
| `MikroTik: Live — Check connectivity --dry-run (opt-in)` | same + `--dry-run` | `allow_concurrent_runs:true` |
| `MikroTik: Deploy current file (REST)` | `python3 scripts/mikrotik-deploy.py $ZED_FILE --method rest` | `save:current`; needs host/pass/`requests` |
| `MikroTik: Deploy current file (SSH)` | `python3 scripts/mikrotik-deploy.py $ZED_FILE --method ssh` | `save:current`; needs host/pass/`paramiko` |

Workflow order is Validate → Check → Deploy → verify; mirror enforced by `tests/test_tasks_mirror.py` (both task files byte-identical, no secrets in env). Windows: use `python` if `python3` is not your launcher.

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `timeout` / `Live FAIL: network error` | Device unreachable or `TIMEOUT` too low | `mikrotik-live-check.py --dry-run` to preview URL; raise `--timeout` (1..30s clamp); `RSC_LS_LOG=debug zed --foreground` |
| `SSRF deny` / `invalid host` | Link-local/metadata target, or host contains `@?#% /` | Use the real device IP/hostname; LAN targets need `RSC_LS_LIVE_ALLOW_LOOPBACK=1` |
| `live enabled but inactive — missing MIKROTIK_HOST/PASS` | Env not visible to the server process | Use `lsp.rsc-ls.binary.env` or a shell profile, then restart Zed |
| `host validation failed` / `exceeds 253 chars` | Bad `MIKROTIK_HOST` | Trim, remove scheme/path, keep the bare host/IP |
| `TLS mismatch` / `certificate verify failed` | Self-signed device or plain HTTP | `MIKROTIK_SSL=0`, or pin via `MIKROTIK_FINGERPRINT` / `MIKROTIK_CA_FILE`; `--no-ssl-verify` never selects scheme |
| `missing MIKROTIK_PASS` (exit 2) | Pass not in env and no TTY prompt | Export it in the shell profile; `--dry-run` needs no pass; never put it in `tasks.json` or settings |
| `response too large` | Device returned more than `MAX_LIVE_RESPONSE_BYTES` | Filter on the device side; the cap protects the LSP |
| `auth failed` / 401/403 | Wrong credentials, REST service disabled, or missing `rest-api` policy | Check `/ip/service` (`www-ssl` enabled + certificate) and the user policy |
| `paramiko` / `requests` missing | Optional deps not installed | `pip install requests paramiko`; dry-run works without them |

## Verification

```bash
# Preview without touching a device (no pass, no network; host still required)
MIKROTIK_HOST=192.168.88.1 python scripts/mikrotik-deploy.py path/to/file.rsc --dry-run
python scripts/mikrotik-live-check.py --dry-run --host 192.168.88.1 --json

# Real check — exit 0 OK, 2 usage, 4 live fail
MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret python scripts/mikrotik-live-check.py --json

# Live enrichment end-to-end (env must be visible to the server; restart Zed)
RSC_LS_LIVE=1 MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret zed --foreground
# then in Zed: open .rsc → trigger interface/address completion → zed: open log → [rsc-ls]
```

Check `lsp/src/caps.rs` for authoritative caps; never hardcode RouterOS version — see `data/commands.toml` header.

## Related Skills

* `language-server` — LSP internals (stale-while-revalidate, coalescing, `build_rest_url`, rustls).
* `development-workflow` — `make` targets, `make validate`, `zed: open log`, `RSC_LS_LOG`, PATH trust model.
* `zed-extension-dev` — publishing, `extension.toml` `rev`, WASM shim.
* `docs-maintenance` — user docs layer (`docs/configuration.md`, `docs/live-enrichment.md`, `docs/device-deploy.md`); link pages, never duplicate env tables here.
