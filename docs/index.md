# MikroTik RouterOS Script — Documentation

Zed extension giving RouterOS scripts (`.rsc`) first-class editor support:
tree-sitter highlighting plus a native language server with completion,
hover, and diagnostics. Live device enrichment is opt-in and in-memory only.

> Volatile facts (pinned grammar rev, extension version, command coverage,
> toolchain) are **never pasted here** — follow the links to the canonical file.

## The three components

| Component | Location | Docs |
| --- | --- | --- |
| Tree-sitter grammar | `grammars/rsc/` (untracked working copy, rev pinned in `extension.toml`) | [grammar.md](grammar.md) |
| Zed language definition | `languages/rsc/` (queries, `config.toml`, `tasks.json`) | [quickstart.md](quickstart.md), [device-deploy.md](device-deploy.md) |
| Language server (`rsc-ls`) | `lsp/src/` (pure Rust, embeds `data/commands.toml`) | [language-features.md](language-features.md), [lsp-config.md](lsp-config.md) |

`src/lib.rs` is the WASM shim Zed loads. It holds zero language logic —
it only resolves `rsc-ls` at runtime (PATH → cache → GitHub Releases).
See [quickstart.md](quickstart.md#2-binary-resolution).

## Start here

- New? [Quickstart](quickstart.md) — install, first `.rsc`, the 3-try loop.
- Writing scripts? [Language features](language-features.md) — what completion,
  hover, and diagnostics actually do.
- Tuning or debugging the server? [LSP config & caps](lsp-config.md),
  [Troubleshooting](troubleshooting.md).
- Real router involved? [Live enrichment](live-enrichment.md) (opt-in),
  [Device deploy](device-deploy.md).
- Contributing? [Grammar](grammar.md), [Data pipeline](data-pipeline.md),
  [QA · CI · Release](qa-ci-release.md).

## Canonical references (volatile facts live here, not in docs)

- Grammar rev: `extension.toml` → `[grammars.rsc] rev`
- Command coverage: `data/commands.toml` header; upstream version: `llms-full.txt` header
- Toolchain / WASM target: `rust-toolchain.toml`; commands: `make help`
- Extension version: `extension.toml`; changelog: `CHANGELOG.md`; roadmap: `ROADMAP.md`

## Further reading

- [Publishing runbook](publishing-runbook.md) — Zed marketplace submission/updates.
- [Architecture records](adr/) — numbered decisions.
- Upstream CLI reference: <https://manual.mikrotik.com/docs/cli-reference/>
- Agent deep dives: `.agents/skills/` (see `AGENTS.md`).
