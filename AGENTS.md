# AGENTS.md

Operating guide for agents and contributors. User docs: `README.md` + `docs/index.md` (entrypoint). Deep dives: `.agents/skills/` — load the matching skill before any non-trivial change.

## What this repo is

Zed extension for MikroTik RouterOS scripts: tree-sitter highlighting + native LSP (completion, hover, diagnostics). Live enrichment is opt-in (`RSC_LS_LIVE=1` / `MIKROTIK_LIVE=1`), in-memory TTL cache only, defensive caps.

| Component | Location | Notes |
| --------- | -------- | ----- |
| Tree-sitter grammar | `grammars/rsc/` | UNTRACKED working copy of [tree-sitter-rsc](https://github.com/balakar94/tree-sitter-rsc) — own repo/lifecycle, pinned via `extension.toml` `rev` |
| Zed language definition | `languages/rsc/` | Queries (highlights, brackets, indents, outline), `config.toml`, `tasks.json` |
| Language server (`rsc-ls`) | `lsp/src/` | Pure-Rust LSP binary; embeds `data/commands.toml` via `include_str!()` |

Glue: `src/lib.rs` is the WASM shim Zed loads — zero language logic; resolves `rsc-ls` at runtime (PATH → cache → GitHub Releases auto-download).

## Hard rules — breaking any of these breaks registry review or runtime behavior:

1. **Never clone or build `zed-industries/zed`.** Depend only on `zed_extension_api`.
2. **Never bundle an `rsc-ls` binary** in the repo or packaged extension. It is resolved at runtime.
3. **Extension `id`/`name` must not contain "zed" or "extension"** (registry policy). Values live in `extension.toml`.
4. **All Rust must compile for `wasm32-wasip2`.** In `src/lib.rs`: no `std::env::var`, no `cfg(...)` — use `zed_extension_api::current_platform()` and `Worktree` methods.
5. **Edit inputs, not generated outputs.** Generated: `src/parser.c`, `data/commands.toml`, `grammars/rsc/src/*`. Change the generator input, then regenerate.
6. **Two independent sources of truth:** grammar semantics from `grammars/rsc/grammar.js`; command data from upstream docs via `llms-full.txt` → `data/commands.toml`. Never couple them.
7. **The LSP stays defensive:** `MAX_MESSAGE_SIZE` 10 MiB / `MAX_HEADER_SIZE` 32 KiB / `MAX_DOC_SIZE` 5 MiB / `MAX_DOCS` 100, bounded diagnostics, strict `file://` URI validation, no filesystem access beyond its cache. Live is opt-in, in-memory only — `LIVE_NEGATIVE_TTL_SECS` 15s, `LIVE_MAX_HOSTS` 4, `LIVE_CUSTOM_RESOURCES_MAX` 8 (`RSC_LS_LIVE_RESOURCES` JSON), SSRF deny (`169.254.169.254`), per-request 5s / blocking 2s (clamped 1..30s).
8. **Everything persisted is English** — code, comments, docs, commits, PRs.
9. Apache-2.0 `LICENSE` stays in the repo root.
10. **`extension.toml` carries only schema-known keys** — validated by `make check-manifest`; unknown keys are silently ignored by Zed and mask typos.

## Daily loop

First clone: `make grammar-clone` (pinned `rev`) + `make install` (`SKIP_SYSTEM=1` skips distro packages). Canonical targets: `make help` — never duplicate the list here.

`make check` — fast compile gate (WASM + LSP). `make validate` — offline gate (manifest, docs, generate-check, fmt, clippy, all tests, extract; upstream staleness is separate: `make sync-check`; extract idempotency fails on dirty `data/commands.toml`). Suites: `make test-grammar` · `make test-rust` · `make test-python`.

### Minimum verification

| You changed… | Run before claiming done |
| ------------ | ------------------------ |
| `lsp/src/**` | `make fmt clippy test-rust` |
| `src/lib.rs` (shim) | `make check-wasm clippy`, then _Install Dev Extension_ in Zed, watch `zed: open log` |
| `grammars/rsc/grammar.js` | In `grammars/rsc/`: `npx tree-sitter generate && npx test`; bump pointer (see _Release_) |
| `languages/rsc/highlights.scm` | Copy to `grammars/rsc/queries/highlights.scm` (only mirrored file; gate: `test_highlights_deduped`), then smoke-test in Zed |
| Extraction / `llms-full.txt` | `make extract`, diff `data/commands.toml`, spot-check vs upstream CLI reference |
| `docs/**` | `make docs-check` (hygiene, links, index reachability, volatile-fact ban) |

`brackets`/`indents`/`outline.scm` are Zed-side only; `injections.scm` lives grammar-side only (intentionally empty, wired via `tree-sitter.json`).

## LSP conventions (`feat/lsp-quality-pass`)

- Completion is relevance-ranked with `textEdit`: deterministic `sortText` tiers (`0!live_` device truth first), `filterText` never filters; ranks stay stable before truncating to `MAX_COMPLETION_ITEMS`.
- Hover cards share `text_util` caps/dedup; typed validators + `SuggestBudget` bound diagnostic quick-fixes; symbols/navigation/folding are hard-capped — `lsp/src/caps.rs` is the single source of truth. Tests: `lsp/src/tests/<module>_<aspect>.rs`; perf budgets in `perf.yml` (schedule-only).
- Privileged transport settings apply only with `RSC_LS_ALLOW_SETTINGS_TRANSPORT=1` (env always wins; legacy shim off by default). Never log secrets.

## Comment style (`lsp/` Rust)

- File header: `// ── Title ──` banner at exactly 76 cols, one blank line, then prose. `//` everywhere (never `//!`); no history refs, no volatile line numbers.
- Section banners follow the same 76-col rule. Prose ≤100 cols; `-` unordered / `1.` ordered lists with text-aligned continuations; blank lines around lists and paragraphs, never double.
- `///` item docs: summary first, `backticks` for code. Markdown tables and URLs are exempt from wrapping.
- Behavior descriptions must match current code (tiers, severities, gates); caps values live in `caps.rs`, never duplicated.

## Data pipeline

`manual.mikrotik.com ──sync_llms.py──▶ llms-full.txt (untracked) ──extract_commands.py──▶ data/commands.toml (tracked, generated) ──include_str!()──▶ rsc-ls`

- Regenerate: `make sync` then `make extract`. CI gates staleness via a timestamp-agnostic `data/commands.toml` diff; hard-fails only when extraction inputs changed and the upstream fetch failed.
- `data/commands.toml` header (version, UTC timestamp, source SHA256) + `data/upstream-docs.toml` provenance manifest are generated — never strip or hand-edit either.
- Weekly `docs-drift` workflow re-checks upstream, notifies via the `upstream-docs` labeled issue (auto-closed on re-sync). Verify against the CLI reference or `/export` on a real router.

## Volatile facts — look up, never quote

| Fact | Canonical location |
| ---- | ------------------ |
| Pinned grammar revision | `extension.toml` → `[grammars.rsc] rev` |
| Command/menu coverage + RouterOS snapshot | header of `data/commands.toml` |
| Upstream doc version | header of `llms-full.txt` |
| Make targets | `make help` |
| Test counts / status | run the suite (`lsp/src/tests/`, `tests/`) |
| MSRV / toolchain / WASM target | `rust-toolchain.toml` |
| Dependency versions | the relevant `Cargo.toml` |

## Repo map (one level)

- `grammars/rsc/` — tree-sitter grammar (untracked working copy)
- `languages/rsc/` — Zed queries + `config.toml` + `tasks.json`
- `lsp/src/` — `rsc-ls`: server (+ `server_proto`/`server_publish`), completion, hover, diagnostics, symbols, navigation, folding, live (`live_config`/`live_net`/`live_cache`/`live_fetch` behind the `live.rs` facade), `caps.rs`, `tests/`
- `src/lib.rs` — WASM shim; `data/` — `commands.toml` (generated) + `upstream-docs.toml` (provenance)
- `scripts/` — sync/extract, `publish_grammar.py`, deploy + live-check (`_mikrotik_shared.py`: SSRF/TLS/redaction parity), `check_docs.py` (docs gate)
- `tests/` — Python suite; `docs/` — user docs (`index.md` entrypoint), runbook, `adr/`; `.agents/skills/` — deep dives (below)

Untracked locals: `llms.txt`, `llms-full.txt` (`make sync`), `extension.wasm`, build output. Validate Live REST first: `scripts/mikrotik-live-check.py`.

## Release

- **Grammar:** `python scripts/publish_grammar.py --dry-run`, then `--push` — validates generation, pushes the working copy, updates `extension.toml rev` itself. **Never hand-edit `rev`.**
- **Version:** `make bump VERSION=x.y.z` (fmt + syncs `Cargo.toml`/`lsp/Cargo.toml`/`extension.toml` + coherence checks; see `CHANGELOG.md`). Grammar crate/package versions are independent — coherence per-group, never cross-group.
- **Binaries:** pushing a `v*.*.*` tag triggers `.github/workflows/release.yml` (multi-platform `rsc-ls` + WASM → GitHub Release).
- **Registry:** follow `docs/publishing-runbook.md` (one extension per PR, ≤3 open, reply ≤3 weeks).

## Device deploy (optional, local only)

- `scripts/mikrotik-deploy.py` — push `.rsc` over REST/SSH. Required: `MIKROTIK_HOST`, `MIKROTIK_USER`, `MIKROTIK_PASS`; optional: `MIKROTIK_PORT`, `MIKROTIK_SSL`, `MIKROTIK_TIMEOUT`, `MIKROTIK_ACCEPT_HOST_KEY`, `MIKROTIK_METHOD`. Always `--dry-run` first. Never log `MIKROTIK_PASS`.
- `scripts/mikrotik-live-check.py` — health check (`GET /rest/interface`; 5s default, 1..30s clamp; `--dry-run`/`--json`; exit 0 OK / 2 usage / 4 live fail). Never log pass.
- Tasks (`languages/rsc/tasks.json`): deploy REST/SSH, dry-run, syntax check, live connectivity check, live enable hint — honest pre-checks before deploy/live. Live opt-in via `RSC_LS_LIVE=1`/`MIKROTIK_LIVE=1` (`MIKROTIK_HOST`/`MIKROTIK_PASS` from env/keychain; `RSC_LS_LIVE_RESOURCES` JSON, max 8 `{"property","path","field"}`).

## Deep dives (`.agents/skills/`)

| Skill | Load when… |
| ----- | ---------- |
| `development-workflow.md` | Day-to-day commands, build/CI failures |
| `tree-sitter-grammar.md` | Grammar edits, corpus failures, publishing |
| `routeros-reference.md` | Command/property lookup, validating command data |
| `language-server.md` | Completion/hover/diagnostics, shim, LSP conventions above |
| `commands-extraction.md` | Regenerating `data/commands.toml`, syncing upstream docs |
| `zed-extension-dev.md` | Manifest, packaging, publishing to `zed-industries/extensions` |
| `device-operations.md` | Deploy, live health-check, REST/SSH env, task wiring |
| `qa-ci-release.md` | Tests, CI gates, docs-drift, release validation |
| `docs-maintenance.md` | Docs edits, volatile facts, docs gate |
| `language-convention.md` | English-only convention, RouterOS naming |
