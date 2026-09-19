# Configuration

How to configure `rsc-ls`, live device data, and the deploy companion — and
which knobs Zed actually delivers.

## How settings reach the language server

Zed starts `rsc-ls` through the extension's WASM shim. Two settings
surfaces exist; only one works today:

| Surface | Status | Use for |
| --- | --- | --- |
| `lsp.rsc-ls.binary.env` | **Works** — Zed merges these variables into the server process environment | `RSC_LS_*` and `MIKROTIK_*` variables |
| `lsp.rsc-ls.settings` | **Not delivered** — the extension does not forward workspace configuration to the server | nothing today |

The server can parse an `rsc.live` / `mikrotik`-scoped settings overlay, but
the extension currently exposes no workspace-configuration hook, so
`lsp.rsc-ls.settings` never reaches it. Configure with `binary.env` instead.
A ready-to-copy example lives in `.zed/settings.example.json`.

```json
{
  "lsp": {
    "rsc-ls": {
      "binary": {
        "env": {
          "RSC_LS_LIVE": "1",
          "MIKROTIK_HOST": "192.168.88.1",
          "MIKROTIK_USER": "admin",
          "RSC_LS_LOG": "info"
        }
      }
    }
  }
}
```

Notes:

- Restart Zed after editing settings or shell profile: environment variables
  are captured when the editor starts, not re-read per request.
- **Never put `MIKROTIK_PASS` in a project settings file.** Project settings
  travel with a repository. Put the password in your shell profile (captured
  by Zed at startup) or let the deploy / live-check tasks prompt for it.
- The WASM shim keeps a session cache of the resolved binary; a `binary.env`
  change restarts the server with the new environment.

## Environment variables

### Language server (`rsc-ls`)

| Variable | Default | Meaning |
| --- | --- | --- |
| `RSC_LS_LOG` | `info` | Log level: `error` < `warn` < `info` < `debug` < `trace`; `RUST_LOG` is also honored |
| `RSC_LS_ALLOW_PATH` | deny | `1` opts in to running an unversioned `rsc-ls` found in `PATH`; any other value keeps the verified cache/download path (see [PATH trust model](#path-trust-model)) |
| `RSC_LS_PATH_SHA256` | — | Optional 64-hex pin for the PATH binary; a mismatch or malformed value falls back to the verified cache |
| `RSC_LS_LIVE` / `MIKROTIK_LIVE` | off | `1` enables live device enrichment |
| `RSC_LS_LIVE_RESOURCES` | — | JSON array (max 8) of `{"property","path","field"}` custom live resources |
| `RSC_LS_LIVE_ALLOW_LOOPBACK` | off | `=1` allows loopback/private/LAN device targets — required for RFC 1918 and ULA addresses |
| `RSC_LS_LIVE_DENY_PREFIXES` | — | Operator SSRF deny list: comma-separated IPv4/IPv6 addresses or CIDR prefixes, always denied |
| `RSC_LS_LEGACY_HTTP_SHIM` | off | `=1` restores the historical `port 80 + SSL=0 → http` fallback; prefer `MIKROTIK_HTTP=1` |
| `RSC_LS_ALLOW_SETTINGS_TRANSPORT` | off | Server-side guard that would allow transport keys from workspace settings; has **no effect in Zed today** because settings are not forwarded |

Live timing, cache, and response caps are documented in
[Caps & limits](lsp-config.md) and [Live enrichment](live-enrichment.md#caps).

### Live device (`rsc-ls` and `mikrotik-live-check.py`)

| Variable | Default | Meaning |
| --- | --- | --- |
| `MIKROTIK_HOST` | — (required) | Device IP/hostname. One host for the scripts; `rsc-ls` parses a comma list and fetches the primary only |
| `MIKROTIK_USER` | `admin` | Username; invalid values fall back with a warning |
| `MIKROTIK_PASS` | — (required) | Password. Environment or interactive prompt only — never settings files, never logged |
| `MIKROTIK_PORT` | `443` | REST port (`1..65535`) |
| `MIKROTIK_HTTP` | off | `1` forces plain HTTP (cleartext credentials — for isolated test networks) |
| `MIKROTIK_SSL` | verify | `0` disables certificate verification; it never changes the scheme |
| `MIKROTIK_TIMEOUT` | `5` | Per-request seconds, clamped `1..30` for live; deploy uses `1..300` |
| `MIKROTIK_FINGERPRINT` | — | SPKI pin `sha256:<64 hex>`; malformed input fails closed (live inactive) |
| `MIKROTIK_CA_FILE` | — | Custom CA bundle; takes precedence over the boolean `SSL` flag |

### Deploy companion (`scripts/mikrotik-deploy.py`)

Shares the live variables above with different defaults, plus:

| Variable | Default | Meaning |
| --- | --- | --- |
| `MIKROTIK_METHOD` | `auto` | `rest`, `ssh`, or `auto` (prefers REST when `requests` is installed) |
| `MIKROTIK_IDENTITY` | — | Private key file for SSH auth; omitted by default (password only, no implicit `~/.ssh` keys) |
| `MIKROTIK_ACCEPT_HOST_KEY` | reject | `1` trusts an unknown SSH host key (TOFU); the accepted fingerprint is printed for verification |
| `MIKROTIK_FORCE_DESTRUCTIVE` | off | `1` acknowledges the destructive pre-scan (hard-blocked resets and confirmed removals) |

All variables can be overridden per run by CLI flags — see
[Device deploy](device-deploy.md#flags).

## PATH trust model

The extension resolves the server in this order:

1. `PATH` — **denied by default**. Set `RSC_LS_ALLOW_PATH=1` to allow it.
   When a PATH binary is used, its absolute path and a short hash prefix are
   logged, and a warning states that it bypasses the checksum gate. Set
   `RSC_LS_PATH_SHA256` to pin it; a mismatch falls through to the cache.
2. Verified work-dir cache — a previously downloaded binary, re-hashed
   before spawn; tampered, truncated, or symlinked entries are refused.
3. Auto-download from GitHub Releases — the release tagged with this
   extension's own version (`v<version>`) first, falling back to the latest
   stable release only when that release is missing or lacks this platform's
   asset. Downloads are staged, checksum-verified, made executable, and only
   then installed. A pinned extension version therefore keeps its matching
   server binary even after a newer release is published.

The checksum companion is same-origin and unsigned: it detects corruption
and binds the digest to the expected asset name, but it is **not release
provenance**. Release attestations exist for operators who verify them
separately. If resolution fails, nothing unverified is executed; the error
messages name the failed stage. See
[offline install and checksum failures](troubleshooting.md#offline-install-and-checksum-failures).
