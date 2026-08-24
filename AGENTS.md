# AGENTS.md

Operating guide for agents and contributors working in this repository.
User-facing documentation: [`README.md`](README.md). Task-specific deep dives: [`.agents/skills/`](.agents/skills/) — load the one matching your task before changing anything non-trivial.

## What this repo is

A Zed extension giving MikroTik RouterOS scripts (RouterOS 7.0+) first-class editor support: tree-sitter syntax highlighting plus a native language server with completion, hover, and diagnostics. One monorepo, three components:

| Component | Location | Notes |
|-----------|----------|-------|
| Tree-sitter grammar | `grammars/rsc/` | Git submodule of [tree-sitter-rsc](https://github.com/balakar94/tree-sitter-rsc) — own repo, own lifecycle |
| Zed language definition | `languages/rsc/` | Queries (highlights, brackets, indents, outline), `config.toml`, `tasks.json` |
| Language server (`rsc-ls`) | `lsp/src/` | Pure-Rust LSP binary; embeds `data/commands.toml` via `include_str!()` |

Glue: `src/lib.rs` is the WASM shim Zed actually loads. It contains zero language logic; it only resolves `rsc-ls` at runtime (PATH → cache → GitHub Releases auto-download).

## Hard rules

Breaking any of these breaks Zed registry review or runtime behavior:

1. **Never clone or build `zed-industries/zed`.** Depend only on `zed_extension_api`.
2. **Never bundle an `rsc-ls` binary** in the repo or the packaged extension. It is resolved at runtime.
3. **Extension `id` and `name` must not contain "zed" or "extension"** (Zed registry policy). Current values live in `extension.toml`.
4. **All Rust must compile for `wasm32-wasip2`.** In `src/lib.rs`: no `std::env::var`, no `cfg(...)` — use `zed_extension_api::current_platform()` and `Worktree` methods.
5. **Edit inputs, not generated outputs.** Generated: `src/parser.c`, `data/commands.toml`, `grammars/rsc/src/*`. Change the generator input, then regenerate.
6. **Two independent sources of truth:** grammar semantics come from `grammars/rsc/grammar.js`; command data comes from upstream docs via `llms-full.txt` → `data/commands.toml`. Separate pipelines — never couple them.
7. **The LSP stays defensive:** capped message/doc/diagnostic sizes, bounded tracked documents, strict `file://` URI validation, no filesystem access beyond its cache.
8. **Everything persisted is English** — code, comments, docs, commits, PRs.
9. Apache-2.0 `LICENSE` stays in the repo root.

## Daily loop

First clone:

```bash
git submodule update --init --recursive   # pulls grammars/rsc
make install                              # full bootstrap (SKIP_SYSTEM=1 skips distro packages)
```

`make help` is the canonical command list — do not maintain a duplicate anywhere. The two you will actually use:

```bash
make check      # fast compile gate (WASM + LSP)
make validate   # full pre-commit/pre-PR gate: generate-check, fmt, clippy, all tests, sync-check, extract
```

Individual suites: `make test-grammar` · `make test-rust` · `make test-python`.

### Minimum verification by change type

| You changed… | Run before claiming done |
|--------------|--------------------------|
| `lsp/src/**` | `make fmt clippy test-rust` |
| `src/lib.rs` (shim) | `make check-wasm clippy`, then *Install Dev Extension* in Zed and watch `zed: open log` |
| `grammars/rsc/grammar.js` | inside `grammars/rsc/`: `npx tree-sitter generate && npx test`; then bump pointer (see *Release*) |
| `languages/rsc/*.scm` | mirror into `grammars/rsc/queries/` (deduped copy must stay in sync), then smoke-test in Zed |
| Extraction pipeline or `llms-full.txt` | `make extract`, diff `data/commands.toml`, spot-check against <https://manual.mikrotik.com/docs/cli-reference/> |

## Data pipeline

```
manual.mikrotik.com ──sync_llms.py──▶ llms-full.txt ──extract_commands.py──▶ data/commands.toml ──include_str!()──▶ rsc-ls
                                       (untracked)                          (tracked, generated)
```

- Regenerate with `make sync` then `make extract`. CI gates staleness via `make sync-check`.
- `data/commands.toml` carries a metadata header (RouterOS version, UTC timestamp, source SHA256). Never strip or hand-edit it.
- Trust but verify: cross-check extracted commands against the upstream CLI reference or `/export` on a real router.

## Volatile facts — look them up, never quote from memory

These change constantly. Re-check at the canonical location before pasting them into any doc, PR, or answer:

| Fact | Canonical location |
|------|--------------------|
| Pinned grammar revision | `extension.toml` → `[grammars.rsc] rev` |
| Available make targets | `make help` |
| Test counts / status | run the suite |
| Command/menu coverage | header of `data/commands.toml` |
| Upstream doc version | header of `llms-full.txt` |
| MSRV / toolchain / WASM target | `rust-toolchain.toml` |
| Dependency versions | the relevant `Cargo.toml` |

## Repo map (one level)

```
├── grammars/rsc/          # Tree-sitter grammar (submodule → tree-sitter-rsc)
├── languages/rsc/         # Zed queries + config.toml + tasks.json
├── lsp/src/               # rsc-ls: main, server, menus, completion, hover, diagnostics
├── src/lib.rs             # WASM shim: resolve/download rsc-ls
├── data/commands.toml     # Generated command table (embedded in rsc-ls)
├── scripts/               # sync_llms, extract_commands, publish_grammar, mikrotik-deploy, test generators
├── tests/                 # Python suite (environment, extraction, functionality, enclosure, release)
├── docs/adr/              # Architecture decision records (numbered, lightweight)
└── .agents/skills/        # Deep-dive guides (see below)
```

Untracked locals: `llms.txt`, `llms-full.txt` (fetch via `make sync`), `extension.wasm`, build output.

## Release

- **Grammar:** `python scripts/publish_grammar.py --dry-run`, then `--push`. It validates generation, pushes the submodule content to GitHub, and updates the pinned revision in `extension.toml` itself — **never hand-edit `rev`**.
- **Version:** `make bump VERSION=x.y.z`. Grammar crate/package.json versions (`grammars/rsc/`) are independent of extension releases — version coherence is enforced only within each group, never across groups.
- **Binaries:** pushing a `v*.*.*` tag triggers `.github/workflows/release.yml` (multi-platform `rsc-ls` + WASM → GitHub Release).

## Device deploy (optional, local only)

`scripts/mikrotik-deploy.py` pushes script files to a device over REST or SSH. Required env vars: `MIKROTIK_HOST`, `MIKROTIK_USER`, `MIKROTIK_PASS`; optional: `MIKROTIK_PORT`, `MIKROTIK_SSL`, `MIKROTIK_METHOD`, `MIKROTIK_TIMEOUT`, `MIKROTIK_ACCEPT_HOST_KEY`. Always `--dry-run` first. Wired as Zed tasks via `languages/rsc/tasks.json`.

## Deep dives (`.agents/skills/`)

| Skill | Load when… |
|-------|------------|
| `development-workflow.md` | Running day-to-day commands, debugging build/CI failures |
| `tree-sitter-grammar.md` | Editing the grammar, corpus failures, publishing it |
| `routeros-reference.md` | Looking up RouterOS commands/properties or validating command data |
| `commands-extraction.md` | Regenerating `data/commands.toml`, syncing upstream docs |
| `language-server.md` | Working on completion/hover/diagnostics or the WASM shim |
| `zed-extension-dev.md` | Extension manifest, packaging, publishing to `zed-industries/extensions` |
| `language-convention.md` | Anything about the English-only convention or RouterOS naming |
