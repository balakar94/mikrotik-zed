# Troubleshooting

Symptom-first. For configuration mechanics see
[Configuration](configuration.md); for the server's own limits see
[Caps & limits](lsp-config.md).

## LS not starting

1. Command Palette → `zed: open log` (and `zed: open language server logs`);
   look for `[rsc-ls]` / `[mikrotik-zed]` lines.
2. `RSC_LS_LOG=debug zed --foreground` for verbose logs
   (levels `error..trace`, default `info`).
3. Confirm the binary resolves: `which rsc-ls`, or `rsc-ls --version` if you
   installed it yourself.
4. Dev extension must point at this directory (*Install Dev Extension*).

## PATH and GUI Zed

The default resolution order is **verified cache → auto-download
(version-pinned release, latest-stable fallback) → no PATH** (PATH is denied
by default). Most users never need PATH at all — the shim downloads a
verified binary on first use.

For local/developer builds:

- Set `RSC_LS_ALLOW_PATH=1`; otherwise a `target/release/rsc-ls` on PATH is
  ignored in favor of the verified cache.
- Optionally pin the binary with `RSC_LS_PATH_SHA256`.

If terminal-launched Zed works but Dock/launcher-launched Zed does not, the
GUI process cannot see the PATH entry. Per platform:

| Platform | `make install-lsp` target | If the GUI still misses it |
| --- | --- | --- |
| macOS Apple Silicon (Homebrew) | `~/.cargo/bin` + `/opt/homebrew/bin` | Ensure the dir is in the login-shell PATH; restart Zed |
| macOS Intel (Homebrew at `/usr/local`) | `~/.cargo/bin` only | Copy manually to `/usr/local/bin` or add your build dir to the login-shell PATH |
| Linux | `~/.cargo/bin` + `~/.local/bin` (if present) | GUI sessions may not include `~/.local/bin`; copy to `/usr/local/bin` or extend the session PATH |
| Windows | — (auto-download handles installs) | Manual installs keep the `.exe` suffix in PATH |

Terminal-launched Zed works but Dock-launched does not ⇒ this is the cause.
Configuration details: [PATH trust model](configuration.md#path-trust-model).

## Settings not applied

- `lsp.rsc-ls.binary.env` is the only surface that reaches the server;
  `lsp.rsc-ls.settings.*` is not forwarded today
  ([why](configuration.md#how-settings-reach-the-language-server)).
- Environment variables are captured at Zed startup — restart the editor
  after editing settings or a shell profile.
- A password inside a project settings file is never used; move it to your
  shell profile or let the tasks prompt.

## Live enrichment fails

1. Run the health check: `python scripts/mikrotik-live-check.py --dry-run`
   then without `--dry-run` (exit **0** OK / **2** usage / **4** live FAIL).
2. Look for the exact reason in the log — `live enabled but inactive — …`:

   | Logged reason | Fix |
   | --- | --- |
   | opt-in not set | Set `RSC_LS_LIVE=1` (or `MIKROTIK_LIVE=1`) |
   | missing `MIKROTIK_HOST` | Set the host; one host for the scripts |
   | missing `MIKROTIK_PASS` | Export the password in the login-shell profile, or use the task prompt |
   | host denied by live policy | LAN/RFC 1918/ULA targets need `RSC_LS_LIVE_ALLOW_LOOPBACK=1`; loopback/LAN are denied by default |
   | invalid `MIKROTIK_PORT` | Use `1..65535` |
   | invalid `MIKROTIK_FINGERPRINT` | Fix or remove the pin — malformed pins fail closed |

3. Other common causes: wrong scheme (`MIKROTIK_HTTP=1` for port-80 plain
   HTTP), self-signed certificate (`MIKROTIK_SSL=0`, or pin via
   `MIKROTIK_FINGERPRINT` / `MIKROTIK_CA_FILE`), device `www-ssl` disabled or
   user lacks the `rest-api` policy (see
   [index.md#prerequisites](index.md#prerequisites)), SSRF-denied host.
4. Completion still works — live failure degrades to static placeholders
   (negative-cached, coalesced). Details:
   [live-enrichment.md](live-enrichment.md).
5. The `rsc.live.refresh` / `rsc.live.status` commands have no Zed UI
   surface; use the log to inspect status.

## Deploy fails

| Symptom / exit | Likely cause | Fix |
| --- | --- | --- |
| exit 2 `--host or MIKROTIK_HOST is required` | Dry-run still requires a host | Pass `--host` or export `MIKROTIK_HOST`; dry-run needs no password |
| exit 2 `must be a single host` | Comma-separated `MIKROTIK_HOST` | The scripts dial one host; use a single value |
| exit 2 `destructive content detected` | `system reset` / `reset-configuration`, or `remove` | Take a backup (`--backup`), then retry with `--force-destructive` |
| exit 2 `use --method ssh` | REST file fallback is limited to 60 KiB | Use `--method ssh` for long scripts |
| exit 3 | `requests` or `paramiko` missing | `pip install requests paramiko`; dry-run works without them |
| exit 4 `TLS` / `certificate verify failed` | Self-signed device certificate | `MIKROTIK_SSL=0`, or pin with `MIKROTIK_FINGERPRINT`, or `MIKROTIK_CA_FILE` |
| exit 4 `backup /export failed` | `--backup` requested but the device refused | Check the `rest-api`/write policies and free space; the push is aborted by design |
| exit 5 sentinel not observed | RouterOS returned 2xx but the completion marker was absent | Verify device state; use `--no-verify-execute` only on builds that return no `/rest/execute` output |
| exit 5 failure marker | `/import` output contained an error marker | Fix the script; diagnostics flag most of these before deploy |
| SSH host-key refused | Host not in `known_hosts` | Verify the fingerprint, then `--accept-host-key`; the accepted SHA256 is printed |
| `paramiko` auth failure | Password-only default | Set `MIKROTIK_IDENTITY` to use a key file |

Exit-code table: [device-deploy.md#exit-codes](device-deploy.md#exit-codes).

## Offline install and checksum failures

No network is needed for editing once `rsc-ls` is installed: the command
database is compiled in. If the auto-download cannot run or fails
verification, the error names the stage and nothing unverified is executed:

- **Companion fetch/parse failure** — the `<asset>.sha256` could not be
  fetched or is malformed or names a different asset.
- **Digest mismatch** — the downloaded bytes do not match the companion.
  Re-run; if it persists, download the asset and its `.sha256` from the
  release page and verify manually.
- **Too large / unreadable** — refuse and re-download.

Manual install (offline): build `cargo build -p rsc-ls --release` and put the
binary on PATH (with `RSC_LS_ALLOW_PATH=1`), or download a release asset and
place it on PATH. The checksum gate is corruption detection, not provenance —
see [PATH trust model](configuration.md#path-trust-model).

## Windows notes

- The auto-downloaded binary is `rsc-ls-<version>.exe`; manual installs must
  keep the suffix.
- The Zed tasks call `python3`; if your launcher is `python`, edit the task
  copy to match (or install the Python launcher alias).
- `make` targets assume a POSIX shell; use the GitHub Release assets and the
  Python scripts directly instead.

## Still stuck

- `zed: open language server logs` → `rsc-ls`.
- File issues with: extension version (`extension.toml`), grammar rev
  (`extension.toml`), relevant log lines with secrets redacted, and the
  minimal `.rsc` reproducer.
