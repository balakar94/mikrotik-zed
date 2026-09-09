# Troubleshooting

## LS not starting

1. Command Palette → `zed: open log`; look for `[rsc-ls]` / `[mikrotik-zed]` lines.
2. `RSC_LS_LOG=debug zed --foreground` (levels `error..trace`, default `info`;
   see [lsp-config.md](lsp-config.md#logging)).
3. Confirm the binary resolves: `which rsc-ls`; dev override via
   `make build-lsp && make install-lsp`.
4. Dev extension must point at this directory (*Install Dev Extension*).

## GUI Zed ignores PATH

GUI apps (Dock) don't inherit shell PATH. `make install-lsp` copies `rsc-ls`
to a GUI-visible location too — re-run it. Terminal-launched Zed works but
Dock-launched Zed doesn't ⇒ this is the cause.

## Offline fallback

No network is needed for editing: the command database is compiled in.
If auto-download fails (fresh-release CDN 404, no network), install
`rsc-ls` from an existing Release asset or `cargo build -p rsc-ls --release`
and put it on PATH — the shim uses it and skips downloading. Checksum
mismatch aborts cleanly with manual instructions; unverified binaries
never execute.

## Live enrichment fails

1. Run the health check first: `python scripts/mikrotik-live-check.py`
   (exit **0** OK / **2** usage / **4** live FAIL) or `--dry-run` for preview.
2. Common causes: live not enabled (`RSC_LS_LIVE=1` or `MIKROTIK_LIVE=1`),
   missing `MIKROTIK_HOST`/`MIKROTIK_PASS`, wrong scheme
   (`MIKROTIK_HTTP=1` for port-80 plain HTTP), self-signed cert
   (`MIKROTIK_SSL=0`, or pin via `MIKROTIK_FINGERPRINT`), SSRF-denied host.
3. Remember: pass in settings files is ignored with a warning —
   env/keychain is the sole password source. Transport overrides from
   workspace settings need `RSC_LS_ALLOW_SETTINGS_TRANSPORT=1`.
4. Completion still works — live failure degrades to static placeholders
   (negative-cached 15 s, coalesced 2 s). Details: [live](live-enrichment.md).

## Still stuck

- `zed: open language server logs` → `rsc-ls` (add `"RSC_LS_LOG": "debug"`
  to non-secret `lsp.rsc-ls.binary.env` in settings for verbosity).
- File issues with: extension version (`extension.toml`), grammar rev
  (`extension.toml`), relevant log lines with secrets redacted, and the
  minimal `.rsc` reproducer.
