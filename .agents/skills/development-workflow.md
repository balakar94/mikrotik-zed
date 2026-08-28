# Skill: Development Workflow

## When to Use

Load this skill when the agent needs to:

- Run day-to-day commands (`make`, `cargo`, `tree-sitter`, `pytest`) or choose the right Makefile target.
- Edit `grammars/rsc/grammar.js`, regenerate `parser.c`, or debug corpus failures.
- Regenerate `data/commands.toml` from `llms-full.txt` or sync upstream docs.
- Iterate on the LSP (`lsp/src/*.rs`, diagnostics, completion, hover) or the WASM extension (`src/lib.rs`).
- Test in Zed (Install Dev Extension, logs, `rsc-ls` PATH vs auto-download).
- Validate before commit/PR or prepare a release (grammar push, 6-target binaries).
- Diagnose CI, formatting, clippy, or deploy-task failures.

For deep dives see: `tree-sitter-grammar`, `commands-extraction`, `language-server`, `zed-extension-dev`, `routeros-reference`.

## Environment Setup

```bash
# Rust 1.90+ (edition 2024) + WASM target for the extension — MSRV matches the zed-industries/extensions registry toolchain
rustup toolchain install 1.90
rustup target add wasm32-wasip2
rustc --version  # >=1.90 (see rust-toolchain.toml)
cargo install cargo-audit  # optional, for `make audit`

# Node 20+ + tree-sitter-cli 0.26.x (grammar only, not at runtime)
npm --prefix grammars/rsc install
npx --prefix grammars/rsc tree-sitter --version  # 0.26.x
# or globally: npm install -g tree-sitter-cli

# Python 3.12+ + deps for extraction, deploy, tests
python3 --version  # >=3.12
pip install pytest requests paramiko tomli  # requests/paramiko for deploy
```

Verify: `cargo check --target wasm32-wasip2 && cargo check -p rsc-ls && cd grammars/rsc && npx tree-sitter generate --help`.

## Makefile Commands

The canonical list is `make help` — do not duplicate it here. Targets worth extra context:

| Command | What it does | When to use |
|---------|-------------|-------------|
| `make parse FILE=path.rsc` | `tree-sitter parse` for a file | Debug a failing parse / inspect node types |
| `make highlight FILE=path.rsc` | `tree-sitter highlight` captures | Debug `highlights.scm` |
| `make generate-check` | Regenerates and `git diff --exit-code` on generated files | CI / pre-commit: ensure `parser.c` committed |
| `make clean-generated` | Same artifact cleanup as `make clean` + removes generated grammar sources | Rare; then `make generate` |
| `make install` / `SKIP_SYSTEM=1 make install` | Full bootstrap (distro deps + toolchains + rsc-ls to PATH) | First setup; skip variant for CI/containers |
| `make validate` | check-manifest + generate-check + fmt + clippy + test-all + extract | One-shot pre-PR gate |
| `make sync-check` | CI-only staleness gate vs upstream docs (exit 2 on drift) — not part of validate | CI; standalone drift check |

Aliases: `make check` expands to `check-wasm` + `check-lsp`. There is no bare `make test` — use `test-grammar` / `test-rust` / `test-python` / `test-all`.

## Workflows

### 1. Grammar Change

1. Edit `grammars/rsc/grammar.js`.
2. `make generate` — regenerates `src/parser.c` etc.
3. `make test-grammar` — corpus tests must pass; inspect `grammars/rsc/test/corpus/*.txt`.
4. `make parse FILE=grammars/rsc/test/example.rsc` and `make highlight FILE=...` for manual check.
5. If corpus expected trees outdated: regenerate in place with the native `cd grammars/rsc && npx tree-sitter test -u` and review the diff.
6. Dedup highlights: `languages/rsc/highlights.scm` is canonical → copy to `grammars/rsc/queries/highlights.scm`.

Verification: `make generate-check` passes, `npx --prefix grammars/rsc tree-sitter test` all green, `rg ERROR grammars/rsc/test/corpus/` has no hits.

### 2. Regenerating `commands.toml`

1. Optionally refresh docs: `python scripts/sync_llms.py --check` (exit 2 if stale).
2. `make extract` — parses `llms-full.txt` ArgTable XML → `data/commands.toml` with header (version, UTC timestamp, SHA256).
3. Spot-check: `rg -c '^\[\[menus\]\]' data/commands.toml` (compare with previous count); `rg 'path = "/ip/firewall/filter"' data/commands.toml`; `rg 'path = "/certificate"' data/commands.toml`.
4. `make test-python && cargo test -p rsc-ls` — LSP embeds `commands.toml` via `include_str!();` rebuild needed.

Verification: `head -20 data/commands.toml` shows fresh header, `pytest tests/test_extract_commands.py -v` passes, no charset/length validation warnings.

### 3. Syncing Upstream Docs (`sync_llms`)

1. `python scripts/sync_llms.py --help` — flags: `--check` (CI, exits 2 if updates), `--force`.
2. `python scripts/sync_llms.py --check` — dry diff via SHA256, no write.
3. `python scripts/sync_llms.py` — fetches `https://manual.mikrotik.com/llms.txt` + `llms-full.txt` (timeout 30 s, retries), writes on change.
4. If updated: `make extract` then `make test-all`.

Verification: `sha256sum llms-full.txt` changes, CI `sync_llms --check` would have flagged, `rg -c menus data/commands.toml` reflects new RouterOS version.

### 4. Zed Dev Iteration (Extension + LSP + Diagnostics)

1. `make build-lsp && export PATH="$PWD/target/release:$PATH"` — PATH fallback for `rsc-ls`.
2. `open -a Zed` (GUI must inherit PATH) or `zed --foreground` from same shell.
3. Zed → Command Palette → `Install Dev Extension` → select `mikrotik-zed/`.
4. Open a `.rsc` file; exercise completion (`/ip` → TAB), hover (menu/property/verb), and diagnostics (7 rules: `unknown-menu`, `unknown-property`, `missing-required`, `duplicate-property`, `invalid-enum-value`, `unclosed-brace`/`unmatched-brace`, `unclosed-quote`).
5. Logs: `zed: open log` or `RSC_LS_LOG=debug zed --foreground` — look for `[mikrotik-zed]` / `[rsc-ls]` prefixes; caps: `MAX_DOC_SIZE 5MiB`, `MAX_MESSAGE_SIZE 10MiB`, `MAX_DIAG_LINES 3000` / `500KB` (`MAX_DIAG_BYTES`).
6. Tasks: `cp languages/rsc/tasks.json .zed/tasks.json`; set `MIKROTIK_HOST/USER/PASS/PORT/SSL/METHOD`; in Zed `task: spawn` → 6 tasks (deploy REST/SSH/dry-run/validate + Live check/enable). Companions: `scripts/mikrotik-deploy.py` (push `.rsc` over REST/SSH) + `scripts/mikrotik-live-check.py` (Live `GET /rest/interface`, 5s default clamped 1..30s, `--dry-run`/`--json`, never logs pass).

Verification: `worktree.which("rsc-ls")` resolves, `textDocument/publishDiagnostics` fires on save, `scripts/mikrotik-deploy.py --dry-run` prints `shlex.quote`'d commands; `scripts/mikrotik-live-check.py --dry-run` validates env without network.

### 5. Release (day-to-day view)

Canonical single source: `AGENTS.md` → *Release*; full extension checklist lives in `zed-extension-dev.md`. This section keeps only the daily steps:

1. Grammar (if `grammar.js` changed): `python scripts/publish_grammar.py --dry-run`, then `--push` — pushes `grammars/rsc` to `balakar94/tree-sitter-rsc` and updates `extension.toml` `rev` (never hand-edit).
2. Bump: `make bump VERSION=x.y.z` — syncs `Cargo.toml`/`lsp/Cargo.toml`/`extension.toml`, runs `cargo fmt` + coherence checks (grammar crate versions stay independent).
3. Docs: update `CHANGELOG.md` (move `Unreleased` → versioned `Fixed`/`Changed`/`Added`, fix compare links) and `ROADMAP.md` (`Now — 0.5.x` tag/hash) — required before every commit (see Pre-commit Checklist).
4. Validate: `make validate && cargo audit` — all green.
5. Tag: `git tag v0.x.y && git push origin v0.x.y` → `.github/workflows/release.yml` on `v*.*.*` builds 6 `rsc-ls` triples (macOS/Linux/Windows × 2 arches) + `extension.wasm` + `*.sha256`; Linux `aarch64-unknown-linux-gnu` builds natively on `ubuntu-24.04-arm` (no `zig`).

Verification: `gh release view v0.x.y --json assets --jq '.assets[].name'` lists 6–7 assets; `extension.toml` rev resolves; `make generate-check` passes on tag.

## Troubleshooting

| # | Error | Cause | Fix |
|---|-------|-------|-----|
| 1 | `npx tree-sitter generate` fails (syntax error, missing `tree-sitter-cli`) | Bad `grammar.js` or no `node_modules` | `node -c grammars/rsc/grammar.js`; `npm --prefix grammars/rsc install`; `npx --prefix grammars/rsc tree-sitter --version` (0.26.x) |
| 2 | `tree-sitter test` corpus failures (`ERROR`/`MISSING` nodes) | Grammar change or stale expected trees | `make parse FILE=<failing>.rsc`; `cd grammars/rsc && npx tree-sitter test -u`, review the diff, commit |
| 3 | `make generate-check` fails / `parser.c` stale in CI | `parser.c` not committed after `grammar.js` edit | `make generate && git add grammars/rsc/src/ && git commit` |
| 4 | `rsc-ls` download 404 / `Failed to download rsc-ls` | No GitHub Release asset for current `version` + `triple` | `gh release view v$(grep version Cargo.toml \| head -1)` check `rsc-ls-<triple>`; fallback `cargo build -p rsc-ls --release && export PATH="$PWD/target/release:$PATH"` then restart Zed from that shell |
| 5 | WASM `extension.wasm` stale / `Failed to compile grammar` in Zed | Old `parser.c` or cached extension | `make generate-check`; `rm -rf ~/Library/Application\ Support/Zed/extensions/installed/mikrotik-rsc` then `Install Dev Extension`; Zed's `extension_builder` rebuilds on install — don't `cp` manually |
| 6 | `Platform not supported for rsc-ls auto-download` | `asset_triple()` has no published binary for that os/arch pair (all six shipped triples are covered) | Build from source: `cargo build -p rsc-ls --release` and put it on PATH; or grab `rsc-ls-<triple>` from GitHub Releases |
| 7 | Diagnostics missing or truncated (large file) | Suppressed lines (`#`, `:global`, `}`, `..`) or caps (`MAX_DIAG_LINES 3000`, `MAX_DIAG_BYTES 500KB`) | Check `lsp/src/diagnostics.rs`; `RSC_LS_LOG=debug zed --foreground`; split file or fix `source = "rsc-ls"` filter in editor |
| 8 | `make extract` reports unexpected menu count or wrong paths | Stale `llms-full.txt` or extraction regex change (`^[a-z0-9][a-z0-9/_-]*$`) | `python scripts/sync_llms.py --force && make extract`; compare `rg -c '^\[\[menus\]\]' data/commands.toml` with previous count; check header `sha256` and `head -20 data/commands.toml` |

Also: `make clippy` fails → `cargo clippy -- -D warnings` must be clean for both `wasm32-wasip2` and `rsc-ls`; `make audit` needs `cargo install cargo-audit`.

## File Locations Quick Reference

| What | Where |
|------|-------|
| Grammar rules | `grammars/rsc/grammar.js` |
| Generated parser | `grammars/rsc/src/parser.c` (+ `grammar.json`, `node-types.json`) |
| Grammar crate | `grammars/rsc/Cargo.toml`, `package.json`, `tree-sitter.json` |
| Canonical highlights | `languages/rsc/highlights.scm` (deduped to `grammars/rsc/queries/highlights.scm`) |
| Brackets / indents / outline | `languages/rsc/brackets.scm`, `indents.scm`, `outline.scm` (no `injections.scm`) |
| Language config | `languages/rsc/config.toml` (`_`, `-`, `$` word chars) |
| Tasks template / active | `languages/rsc/tasks.json` → `.zed/tasks.json` (6 tasks: deploy REST/SSH/dry-run/validate + Live check/enable) |
| Command table | `data/commands.toml` (header: version, timestamp, sha256) |
| Truth source docs | `llms-full.txt` (version in header), `llms.txt` (index) |
| Extraction / sync | `scripts/extract_commands.py`, `scripts/sync_llms.py` |
| Deploy companion | `scripts/mikrotik-deploy.py` (REST `requests`, SSH `paramiko`, 5MiB cap) |
| Live check companion | `scripts/mikrotik-live-check.py` (REST `GET /rest/interface`, 5s default 1..30s, `--dry-run`/`--json`, never logs pass) |
| Grammar publisher | `scripts/publish_grammar.py` (push + update `extension.toml` rev) |
| Test corpus | `grammars/rsc/test/corpus/*.txt` (regenerate expectations with native `npx tree-sitter test -u`) |
| Extension manifest / WASM | `extension.toml` (grammar `rev`), `src/lib.rs` (auto-download + PATH fallback, `platform_triple`) |
| LSP binary (native) | `lsp/Cargo.toml`, `lsp/src/main.rs` (bootstrap + re-exports), `lsp/src/server.rs` (stdio JSON-RPC: `Server`, dispatch loop, doc store, URI validation), `lsp/src/caps.rs` (all resource limits) |
| LSP modules | `lsp/src/menus.rs` (indices), `completion.rs`, `hover.rs`, `diagnostics.rs` (7 rules capped `MAX_DIAG_LINES 3000` / `500KB`), `live.rs` (opt-in enrichment, TTL/cache caps) + `caps.rs` (defensive limits) |
| Workspace / build | `Cargo.toml` (workspace `lsp`, `wasm32-wasip2`), `Makefile`, `extension.wasm` |
| CI / Release | `.github/workflows/ci.yml`, `release.yml` (6 triples + WASM + GitHub Release, trigger `v*.*.*` tag; native `aarch64` on `ubuntu-24.04-arm` — no `zig`) |

## Pre-commit Checklist (ALWAYS)

> **Never commit without these — the assistant must enforce this.**
> This checklist is the single source of truth for release hygiene.

- [ ] **CHANGELOG.md**: move `## [Unreleased]` to versioned `## [x.y.z] - YYYY-MM-DD` with `### Fixed`/`Changed`/`Added`, update link footnotes (`[Unreleased]: compare/vx.y.z...HEAD`, `[x.y.z]: compare/v...v...`).
- [ ] **ROADMAP.md**: update `Now — 0.5.x` with tag/hash and snapshot, remove redundant principles (e.g., English-only is implicit, not listed), keep `Volatile facts` pointer.
- [ ] **README.md** if version/snapshot changed: update badge note and `Coverage` / `Sync` snapshot line.
- [ ] **Docs sync if needed**: `make sync && make extract` then `head -20 data/commands.toml` + `cat data/upstream-docs.toml` to confirm hash/version.
- [ ] **Validate**: `make validate` (includes `check-manifest` + `generate-check` + `fmt` + `clippy` + `test-all` + `extract` idempotency) — must be green.
- [ ] **Tag trigger note**: `release.yml` only runs on `git push origin v*.*.*` (or `workflow_dispatch`), never on plain `git push`. Verify tag push separately: `git tag vX.Y.Z && git push origin vX.Y.Z`.
