# AGENTS.md

Operating guide for agents and contributors working in this repository.
User-facing documentation: [`README.md`](README.md). Task-specific deep dives: [`.agents/skills/`](.agents/skills/) — load the one matching your task before changing anything non-trivial.

## What this repo is

A Zed extension giving MikroTik RouterOS scripts (RouterOS 7.20+ — see `data/commands.toml` header for snapshot; compatible with 7.0+ for common menus) first-class editor support: tree-sitter syntax highlighting plus a native language server with completion, hover, and diagnostics. Live device enrichment is opt-in (`RSC_LS_LIVE=1` / `MIKROTIK_LIVE=1`), in-memory TTL cache only, defensive caps. One monorepo, three components:

| Component                  | Location         | Notes                                                                                                                                            |
| -------------------------- | ---------------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| Tree-sitter grammar        | `grammars/rsc/`  | UNTRACKED working copy of [tree-sitter-rsc](https://github.com/balakar94/tree-sitter-rsc) since 0.5.0 (refactor(grammar) #33) — own repo, own lifecycle, pinned via `extension.toml rev` |
| Zed language definition    | `languages/rsc/` | Queries (highlights, brackets, indents, outline), `config.toml`, `tasks.json`                                                                    |
| Language server (`rsc-ls`) | `lsp/src/`       | Pure-Rust LSP binary; embeds `data/commands.toml` via `include_str!()`                                                                           |

Glue: `src/lib.rs` is the WASM shim Zed actually loads. It contains zero language logic; it only resolves `rsc-ls` at runtime (PATH → cache → GitHub Releases auto-download).

## Hard rules

Breaking any of these breaks Zed registry review or runtime behavior:

1. **Never clone or build `zed-industries/zed`.** Depend only on `zed_extension_api`.
2. **Never bundle an `rsc-ls` binary** in the repo or the packaged extension. It is resolved at runtime.
3. **Extension `id` and `name` must not contain "zed" or "extension"** (Zed registry policy). Current values live in `extension.toml`.
4. **All Rust must compile for `wasm32-wasip2`.** In `src/lib.rs`: no `std::env::var`, no `cfg(...)` — use `zed_extension_api::current_platform()` and `Worktree` methods.
5. **Edit inputs, not generated outputs.** Generated: `src/parser.c`, `data/commands.toml`, `grammars/rsc/src/*`. Change the generator input, then regenerate.
6. **Two independent sources of truth:** grammar semantics come from `grammars/rsc/grammar.js`; command data comes from upstream docs via `llms-full.txt` → `data/commands.toml`. Separate pipelines — never couple them.
7. **The LSP stays defensive:** capped `MAX_MESSAGE_SIZE` 10 MiB / `MAX_HEADER_SIZE` 32 KiB / `MAX_DOC_SIZE` 5 MiB / `MAX_DOCS` 100, bounded diagnostics, strict `file://` URI validation, no filesystem access beyond its cache. Live is opt-in (`RSC_LS_LIVE=1` / `MIKROTIK_LIVE=1`), in-memory only — caps: `LIVE_NEGATIVE_TTL_SECS` 15s negative cache, `LIVE_MAX_HOSTS` 4, `LIVE_CUSTOM_RESOURCES_MAX` 8 (`RSC_LS_LIVE_RESOURCES` JSON), SSRF deny (`169.254.169.254`), per-request 5s / blocking 2s timeouts (clamped 1..30s).
8. **Everything persisted is English** — code, comments, docs, commits, PRs.
9. Apache-2.0 `LICENSE` stays in the repo root.
10. **`extension.toml` carries only schema-known keys** — validated by `make check-manifest`; unknown keys are silently ignored by Zed and mask typos.

## Daily loop

First clone:

```bash
make grammar-clone                        # pulls grammars/rsc at the pinned rev
make install                              # full bootstrap (SKIP_SYSTEM=1 skips distro packages)
```

`make help` is the canonical command list — do not maintain a duplicate anywhere. The two you will actually use:

```bash
make check      # fast compile gate (WASM + LSP)
make validate   # offline gate: manifest, generate-check, fmt, clippy, all tests, extract
                # (upstream-docs staleness is a separate CI gate: make sync-check)
                # validate includes extract idempotency: fails if `git diff --exit-code data/commands.toml` is dirty
```

Individual suites: `make test-grammar` · `make test-rust` · `make test-python`.

### Minimum verification by change type

| You changed…                           | Run before claiming done                                                                                        |
| -------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `lsp/src/**`                           | `make fmt clippy test-rust`                                                                                     |
| `src/lib.rs` (shim)                    | `make check-wasm clippy`, then _Install Dev Extension_ in Zed and watch `zed: open log`                         |
| `grammars/rsc/grammar.js`              | inside `grammars/rsc/`: `npx tree-sitter generate && npx test`; then bump pointer (see _Release_)               |
| `languages/rsc/*.scm`                  | mirror into `grammars/rsc/queries/` (deduped copy must stay in sync), then smoke-test in Zed                    |
| Extraction pipeline or `llms-full.txt` | `make extract`, diff `data/commands.toml`, spot-check against <https://manual.mikrotik.com/docs/cli-reference/> |

## Data pipeline

```
manual.mikrotik.com ──sync_llms.py──▶ llms-full.txt ──extract_commands.py──▶ data/commands.toml ──include_str!()──▶ rsc-ls
                                       (untracked)                          (tracked, generated)
```

- Regenerate with `make sync` then `make extract`. CI gates staleness via the python job's timestamp-agnostic `data/commands.toml` diff, failing hard only when extraction inputs changed and the upstream `llms-full.txt` fetch failed.
- `data/commands.toml` carries a metadata header (RouterOS version, UTC timestamp, source SHA256). Never strip or hand-edit it.
- `data/upstream-docs.toml` is the sync provenance manifest (SHA256 of both upstream files, RouterOS version, UTC timestamp), regenerated alongside `make sync` — never hand-edit it.
- A weekly `docs-drift` workflow re-checks upstream against that snapshot and notifies via the `upstream-docs` labeled issue, auto-closed once re-synced.
- Trust but verify: cross-check extracted commands against the upstream CLI reference or `/export` on a real router.

## Volatile facts — look them up, never quote from memory

These change constantly. Re-check at the canonical location before pasting them into any doc, PR, or answer:

| Fact                           | Canonical location                      |
| ------------------------------ | --------------------------------------- |
| Pinned grammar revision        | `extension.toml` → `[grammars.rsc] rev` |
| Available make targets         | `make help`                             |
| Test counts / status           | run the suite                           |
| Command/menu coverage          | header of `data/commands.toml`          |
| Upstream doc version           | header of `llms-full.txt`               |
| MSRV / toolchain / WASM target | `rust-toolchain.toml`                   |
| Dependency versions            | the relevant `Cargo.toml`               |

## Repo map (one level)

```
├── grammars/rsc/          # Tree-sitter grammar (untracked working copy since 0.5.0 → tree-sitter-rsc)
├── languages/rsc/         # Zed queries + config.toml + tasks.json (6 tasks)
├── lsp/src/               # rsc-ls: main, server, menus, completion, hover, diagnostics, live.rs, caps.rs
├── src/lib.rs             # WASM shim: resolve/download rsc-ls
├── data/commands.toml     # Generated command table (embedded in rsc-ls)
├── scripts/               # sync_llms.py, extract_commands.py, publish_grammar.py, mikrotik-deploy.py, mikrotik-live-check.py
├── tests/                 # Python suite (environment, extraction, functionality, enclosure, release, live)
├── docs/adr/              # Architecture decision records (numbered, lightweight)
└── .agents/skills/        # Deep-dive guides (see below)
```

Untracked locals: `llms.txt`, `llms-full.txt` (fetch via `make sync`), `extension.wasm`, build output.
Use `scripts/mikrotik-live-check.py` to validate Live REST connectivity before enabling enrichment.

## Release

- **Grammar:** `python scripts/publish_grammar.py --dry-run`, then `--push`. It validates generation, pushes the grammar working copy to GitHub, and updates the pinned revision in `extension.toml` itself — **never hand-edit `rev`**.
- **Version:** `make bump VERSION=x.y.z` (runs `cargo fmt`, syncs `Cargo.toml`/`lsp/Cargo.toml`/`extension.toml`, coherence checks). Grammar crate/package.json versions (`grammars/rsc/`) are independent of extension releases — version coherence is enforced only within each group, never across groups.
- **Binaries:** pushing a `v*.*.*` tag triggers `.github/workflows/release.yml` (multi-platform `rsc-ls` + WASM → GitHub Release). Linux `aarch64-unknown-linux-gnu` now builds natively on `ubuntu-24.04-arm` (no `zig`/`cargo-zigbuild` cross).
- **Registry:** follow [`docs/publishing-runbook.md`](docs/publishing-runbook.md) for submission/update PRs (rules: one extension per PR, ≤3 open, reply ≤3 weeks).

## Device deploy (optional, local only)

Two companion scripts, both never log `MIKROTIK_PASS`:

- `scripts/mikrotik-deploy.py` — push `.rsc` files to a device over REST or SSH. Required env: `MIKROTIK_HOST`, `MIKROTIK_USER`, `MIKROTIK_PASS`; optional: `MIKROTIK_PORT`, `MIKROTIK_SSL`, `MIKROTIK_TIMEOUT`, `MIKROTIK_ACCEPT_HOST_KEY`, `MIKROTIK_METHOD`. Always `--dry-run` first.
- `scripts/mikrotik-live-check.py` — Live health check: authenticated `GET /rest/interface` to verify REST reachability for enrichment. Respects `MIKROTIK_SSL`/`PORT`/`TIMEOUT` (5s default, 1..30s clamp), supports `--dry-run` and `--json`, host validation + SSRF checks, never logs pass. Exit 0 OK / 2 usage / 4 live fail.

Wired as 6 Zed tasks via `languages/rsc/tasks.json` (deploy REST/SSH, dry-run, validate syntax, live check connectivity, live enable hint). Live enrichment itself is opt-in via `RSC_LS_LIVE=1` or `MIKROTIK_LIVE=1` with `MIKROTIK_HOST`/`MIKROTIK_PASS` from env/keychain; custom resources via `RSC_LS_LIVE_RESOURCES` JSON array (max 8, `{"property","path","field"}`).

## Deep dives (`.agents/skills/`)

| Skill                     | Load when…                                                               |
| ------------------------- | ------------------------------------------------------------------------ |
| `development-workflow.md` | Running day-to-day commands, debugging build/CI failures                 |
| `tree-sitter-grammar.md`  | Editing the grammar, corpus failures, publishing it                      |
| `routeros-reference.md`   | Looking up RouterOS commands/properties or validating command data       |
| `commands-extraction.md`  | Regenerating `data/commands.toml`, syncing upstream docs                 |
| `language-server.md`      | Working on completion/hover/diagnostics or the WASM shim                 |
| `zed-extension-dev.md`    | Extension manifest, packaging, publishing to `zed-industries/extensions` |
| `device-operations.md`    | Device deploy, live health-check, REST/SSH env, task wiring              |
| `qa-ci-release.md`        | Tests, CI gates, docs-drift, release validation                          |
| `language-convention.md`  | Anything about the English-only convention or RouterOS naming            |
