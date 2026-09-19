# MikroTik RouterOS Script — Documentation

Zed extension for MikroTik RouterOS scripts (`.rsc`): tree-sitter
highlighting plus a native language server with completion, hover,
diagnostics, and navigation. Live device data is opt-in; deployment to a
device is a separate, optional step.

> Volatile facts (pinned grammar rev, extension version, command coverage,
> toolchain, RouterOS snapshot) are **never pasted here** — follow the links
> to the canonical file.

## Prerequisites

**Editor side**

- Zed with extension support; the language server (`rsc-ls`) is resolved at
  runtime (verified cache → auto-download), so the first `.rsc` open needs
  network unless the binary is already installed.
- Optional for the device workflows: Python 3, `requests` (REST), and
  `paramiko` (SSH). Dry-run previews work without either package.

**RouterOS side (only for Live and Deploy)**

- The REST API is a JSON wrapper over the API service. Enable `www-ssl`
  (HTTPS, recommended) or `www` (plain HTTP, cleartext credentials) in
  `/ip/service`. HTTPS requires a certificate for `www-ssl`.
- The user needs the `rest-api` policy (plus `read`, and `write` for
  changes). Give the LSP/deploy user only the policies it needs.
- REST support exists since the early RouterOS 7 betas (the upstream docs
  show `7.1beta4`); plain-HTTP REST availability varies by release — when in
  doubt use `www-ssl`.
- Verify with `scripts/mikrotik-live-check.py --dry-run`, then without the
  flag. Details: [live enrichment](live-enrichment.md).

## Start here

- New? [Quickstart](quickstart.md) — install, first `.rsc`, the 3-try loop.
- Writing scripts? [Language features](language-features.md) — what
  completion, hover, and diagnostics actually do.
- Tuning or debugging the server? [Configuration](configuration.md),
  [Caps & limits](lsp-config.md), [Troubleshooting](troubleshooting.md).
- Doing a real change on a router? [Recipes](recipes.md) (backup, IP
  address, firewall, DHCP) and [Device deploy](device-deploy.md).
- Real router involved? [Live enrichment](live-enrichment.md) (opt-in).
- Unfamiliar term? [Glossary](glossary.md).

## The three components

| Component | Location | Docs |
| --- | --- | --- |
| Tree-sitter grammar | `grammars/rsc/` (untracked working copy, rev pinned in `extension.toml`) | [grammar.md](grammar.md) |
| Zed language definition | `languages/rsc/` (queries, `config.toml`, `tasks.json`) | [quickstart.md](quickstart.md), [device-deploy.md](device-deploy.md) |
| Language server (`rsc-ls`) | `lsp/src/` (pure Rust, embeds `data/commands.toml`) | [language-features.md](language-features.md), [lsp-config.md](lsp-config.md) |

`src/lib.rs` is the WASM shim Zed loads. It holds zero language logic —
it only resolves `rsc-ls` at runtime (verified cache/download; a PATH lookup
is opt-in). See [PATH trust model](configuration.md#path-trust-model) and
[offline fallback](troubleshooting.md#offline-install-and-checksum-failures).

## Canonical references (volatile facts live here, not in docs)

- Grammar rev: `extension.toml` → `[grammars.rsc] rev`
- Command coverage: `data/commands.toml` header; upstream version: `llms-full.txt` header
- Toolchain / WASM target: `rust-toolchain.toml`; commands: `make help`
- Extension version: `extension.toml`; changelog: `CHANGELOG.md`; roadmap: `ROADMAP.md`

## Further reading

- [Publishing runbook](publishing-runbook.md) — Zed marketplace submission/updates.
- [QA · CI · Release](qa-ci-release.md) — local gates, CI pipelines, release validation.
- [Architecture records](adr/) — numbered decisions.
- Upstream CLI reference: <https://manual.mikrotik.com/docs/cli-reference/>
- Agent deep dives: `.agents/skills/` (see `AGENTS.md`).
