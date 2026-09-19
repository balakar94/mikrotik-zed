# Live device enrichment (opt-in)

Disabled by default. When enabled, `rsc-ls` fetches real device data over
the RouterOS REST API into an **in-memory TTL cache** (never touches
`data/commands.toml` or disk) and surfaces it as top-tier completions
(`0!live_…`, detail `live — …`). Completion never blocks: the background
hydrator is coalesced, failures are negative-cached, and an offline device
falls back to static placeholders. Sources: `lsp/src/live.rs`,
`lsp/src/caps.rs`.

Device-side prerequisites (`www-ssl`, certificate, `rest-api` policy):
[index.md#prerequisites](index.md#prerequisites).

## Enable

The reliable way to reach the server process from Zed is
`lsp.rsc-ls.binary.env` (project or user settings). Env vars exported in a
terminal are not visible to a GUI-launched Zed; a shell profile works after a
restart because Zed captures it at startup.

```json
{
  "lsp": {
    "rsc-ls": {
      "binary": {
        "env": {
          "RSC_LS_LIVE": "1",
          "MIKROTIK_HOST": "192.168.88.1",
          "MIKROTIK_USER": "admin"
        }
      }
    }
  }
}
```

**Do not keep the password in a project settings file** — project settings
travel with the repository. Put `MIKROTIK_PASS` in your shell profile
(captured by Zed at startup), or in a machine-local settings file that is
never committed. The deploy and live-check tasks prompt with `getpass` when
the variable is absent.

Restart Zed after changing settings or profile. A ready-to-copy example lives
in `.zed/settings.example.json` (secret-free).

> **Settings passthrough limitation:** `lsp.rsc-ls.settings.*` is not
> delivered to the server today — the extension exposes no workspace
> configuration hook. Configure only via `binary.env` / environment. See
> [Configuration](configuration.md#how-settings-reach-the-language-server).

Check first:

```bash
python scripts/mikrotik-live-check.py --dry-run --host 192.168.88.1
MIKROTIK_HOST=192.168.88.1 MIKROTIK_PASS=secret \
  python scripts/mikrotik-live-check.py
```

Or use the Zed task *MikroTik: Live — Check connectivity (opt-in)* (the
companion script requires exactly one host — a comma list is a usage error).

### LAN targets need the loopback opt-in

`rsc-ls` denies loopback, RFC 1918, and ULA targets by default (SSRF
hardening). For a router on your LAN, set:

```bash
RSC_LS_LIVE_ALLOW_LOOPBACK=1
```

The health-check script allows private targets itself and prints a warning
when the flag is missing, so a green check does **not** imply the LSP will
activate. Operator deny prefixes (`RSC_LS_LIVE_DENY_PREFIXES`) are checked
first and are never relaxed by this flag.

## Verify it is working

Watch the language server log (`zed: open log`, or
`RSC_LS_LOG=debug zed --foreground`). Success looks like:

```text
[rsc-ls][INFO][T+…s] live enabled host=… port=443 scheme=https user=… ssl_verify=… ssl_verify_effective=true …
[rsc-ls][INFO][T+…s] live status on initialize: enabled=true active=true inactive_reason=none host=… scheme=https …
```

Typing a live-enrichable value (for example `interface=` after
`/ip address add`) then shows device names marked `live — interface on
device`.

Failure is explicit in the log:

```text
[rsc-ls][INFO][T+…s] live disabled (opt-in via RSC_LS_LIVE=1 or MIKROTIK_LIVE=1)
[rsc-ls][INFO][T+…s] live enabled but inactive — <reason>
```

Reasons are named exactly: opt-in not set; missing `MIKROTIK_HOST`; missing
`MIKROTIK_PASS`; host denied by live policy (SSRF/loopback/operator deny);
invalid `MIKROTIK_PORT`; invalid `MIKROTIK_FINGERPRINT` (fail-closed).

> **No Zed UI for cache control:** the server advertises
> `rsc.live.refresh` and `rsc.live.status`, but no Zed surface invokes
> `workspace/executeCommand` for this extension today. The status details
> (including `inactive_reason`) appear in the log only. The cache refreshes
> on reconnect, on `didClose`, and automatically by TTL.

## Full `MIKROTIK_*` env table

| Var | Default | Notes |
| --- | --- | --- |
| `RSC_LS_LIVE` / `MIKROTIK_LIVE` | off (`0`) | either `=1` enables |
| `MIKROTIK_HOST` | — (required) | IP/hostname. `rsc-ls` accepts a comma list and fetches the primary host only; the scripts reject a list |
| `MIKROTIK_USER` | `admin` | invalid values fall back with a warning |
| `MIKROTIK_PASS` | — (required) | **never logged**; environment/prompt only |
| `MIKROTIK_PORT` | `443` | `1..65535` |
| `MIKROTIK_HTTP` | `0` (https) | `=1` forces plain HTTP |
| `MIKROTIK_SSL` | verify on | `=0` disables verification (scheme unchanged; see legacy shim below) |
| `MIKROTIK_TIMEOUT` | `5` | per-request seconds, clamped `1..30` |
| `MIKROTIK_FINGERPRINT` | — | `sha256:<64 hex>` SPKI pin; invalid → fail-closed (inactive) |
| `MIKROTIK_CA_FILE` | — | custom CA bundle path (wins over the boolean flag) |
| `RSC_LS_LIVE_RESOURCES` | — | JSON array max 8, `{"property","path","field"}` custom resources |
| `RSC_LS_LIVE_ALLOW_LOOPBACK` | off | `=1` allows loopback/private/RFC 1918/ULA hosts |
| `RSC_LS_LIVE_DENY_PREFIXES` | — | comma-separated IPv4/IPv6 addresses or CIDR prefixes always denied (env-only) |

Deploy shares the same `MIKROTIK_*` semantics with different defaults
(timeout 60, SSH support) — see [device-deploy.md](device-deploy.md).

## What gets enriched

| Property family | Source on device |
| --- | --- |
| `interface`, `bridge`, ports, `*-interface` (any `iface` type) | `/rest/interface` |
| `in-interface-list`, `out-interface-list`, `list` under `/interface/list` | `/rest/interface/list` |
| `address`, `network`, `gateway`, prefixes | `/rest/ip/address` (IPv6 menu → IPv6 table) |
| firewall `src-address-list`, `dst-address-list`, `address-list` | `/rest/ip/firewall/address-list` (IPv6 family reads IPv6 tables) |
| firewall `chain` (filter, nat, raw) | the family-correct table; IPv6 has no mangle table |
| `pool`, `address-pool`, `pool-name` | `/rest/ip/pool` (IPv6 family → IPv6 pool) |

Fetches request only the needed field (`?.proplist=…`) and cap the response;
custom resources extend the mapping via `RSC_LS_LIVE_RESOURCES`, each
`{"property","path","field"}` with `path` under `/rest`.

## TLS: fingerprint / CA

Precedence: custom CA bundle → SPKI pin (`MIKROTIK_FINGERPRINT`) → boolean
`MIKROTIK_SSL`. A pin or CA counts as verification even with
`MIKROTIK_SSL=0`. TLS 1.2/1.3. A malformed pin fails closed (live inactive,
logged).

Legacy shim (off by default): `RSC_LS_LEGACY_HTTP_SHIM=1` restores the old
`MIKROTIK_SSL=0`-on-nonstandard-port → http downgrade. Prefer
`MIKROTIK_HTTP=1`.

## SSRF deny

Host validation + SSRF denial run before any request
(`169.254.169.254` denied; loopback/private denied without the
allow-loopback flag). Expanded, zero-padded IPv6 literals are accepted and
judged by their normalized address; non-canonical IPv4 spellings (decimal,
hex, octal, short form) are refused. Shared with deploy/live-check via
`scripts/_mikrotik_shared.py`.

`RSC_LS_LIVE_DENY_PREFIXES` extends the built-in policy with an
operator-defined deny list: comma-separated IPv4/IPv6 addresses or CIDR
prefixes, checked **before** the built-in policy and applied regardless of
`RSC_LS_LIVE_ALLOW_LOOPBACK`. Invalid entries and entries past the caps are
ignored with a warning; the list is env-only.

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
| `MAX_LIVE_DENY_PREFIXES` | 32 operator deny prefixes |
| `MAX_LIVE_DENY_PREFIXES_BYTES` | 2 KiB raw deny-prefix env value |
| fetch-thread semaphore | 2 (`live.rs`) |

Invalidation: `didChange` never clears; `didClose`, a connection-identity
change, and TTL expiry do.

## live-check exit codes

`scripts/mikrotik-live-check.py`: **0** Live OK (`Live OK: N interfaces`),
**2** usage error (missing host, comma-separated host list, missing
password), **4** live FAIL (network/auth/status/parse/host validation).
Supports `--dry-run` and `--json`; never logs the password.
