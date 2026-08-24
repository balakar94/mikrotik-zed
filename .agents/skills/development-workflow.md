# Skill: Development Workflow

## When to Use

Load this skill when the agent needs to:

- Run day-to-day commands (`make`, `cargo`, `tree-sitter`, `pytest`) or choose the right Makefile target.
- Edit `grammars/rsc/grammar.js`, regenerate `parser.c`, or debug corpus failures.
- Regenerate `data/commands.toml` from `llms-full.txt` or sync upstream docs.
- Iterate on the LSP (`lsp/src/*.rs`, diagnostics, completion, hover) or the WASM extension (`src/lib.rs`).
- Test in Zed (Install Dev Extension, logs, `rsc-ls` PATH vs auto-download).
- Validate before commit/PR or prepare a release (grammar push, 4-target binaries).
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
| `make clean-generated` | `make clean` + removes generated grammar sources | Rare; then `make generate` |
| `make install` / `SKIP_SYSTEM=1 make install` | Full bootstrap (distro deps + toolchains + rsc-ls to PATH) | First setup; skip variant for CI/containers |
| `make validate` | generate-check + fmt + clippy + test-all + sync-check + extract | One-shot pre-PR gate |

Aliases: `make test` → `test-grammar`; `make check` expands to `check-wasm` + `check-lsp`.

## Workflows

### 1. Grammar Change

1. Edit `grammars/rsc/grammar.js`.
2. `make generate` — regenerates `src/parser.c` etc.
3. `make test` — corpus tests must pass; inspect `grammars/rsc/test/corpus/*.txt`.
4. `make parse FILE=grammars/rsc/test/example.rsc` and `make highlight FILE=...` for manual check.
5. If corpus expected trees outdated: prefer the native `cd grammars/rsc && npx tree-sitter test -u` (review the diff); helpers `scripts/{regenerate,generate,clean}_tests.py` do batch corpus regeneration from parser output.
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
4. Open a `.rsc` file; exercise completion (`/ip` → TAB), hover (menu/property/verb), and diagnostics (5 rules: `unknown-menu`, `unknown-property`, `missing-required`, `duplicate-property`, `invalid-enum-value`).
5. Logs: `zed: open log` or `RSC_LS_LOG=debug zed --foreground` — look for `[mikrotik-zed]` / `[rsc-ls]` prefixes; caps: `MAX_DOC_SIZE 5MiB`, `MAX_MESSAGE_SIZE 10MiB`, `MAX_DIAG_LINES 3000` / `500KB`.
6. Tasks: `cp languages/rsc/tasks.json .zed/tasks.json`; set `MIKROTIK_HOST/USER/PASS/PORT/SSL/METHOD`; in Zed `task: spawn` → MikroTik deploy (REST/SSH/Dry-run); CLI: `python scripts/mikrotik-deploy.py test.rsc --dry-run`.

Verification: `worktree.which("rsc-ls")` resolves, `textDocument/publishDiagnostics` fires on save, `scripts/mikrotik-deploy.py --dry-run` prints `shlex.quote`'d commands.

### 5. Release (Grammar + Binary + GitHub Release)

1. Grammar: `python scripts/publish_grammar.py` — checks `tree-sitter generate` clean, pushes `grammars/rsc` to `balakar94/tree-sitter-rsc`, updates `extension.toml` `rev` (never hand-edit; verify against `extension.toml`).
2. Bump `version` in `Cargo.toml` + `extension.toml`.
3. `make validate && make fmt clippy && cargo audit` — all green.
4. Tag: `git tag v0.x.y && git push origin v0.x.y` → `.github/workflows/release.yml` builds `rsc-ls` for 4 triples (`aarch64/x86_64` × `apple-darwin/linux-gnu`) + WASM, creates GitHub Release with assets `rsc-ls-<triple>`.
5. Extension: PR to `zed-industries/extensions` with submodule + `extensions.toml` + `pnpm sort-extensions`.

Verification: `gh release view v0.x.y --json assets --jq '.assets[].name'` lists 4 binaries, `extension.toml` rev resolves on GitHub, `make generate-check` passes on tag.

## Troubleshooting

| # | Error | Cause | Fix |
|---|-------|-------|-----|
| 1 | `npx tree-sitter generate` fails (syntax error, missing `tree-sitter-cli`) | Bad `grammar.js` or no `node_modules` | `node -c grammars/rsc/grammar.js`; `npm --prefix grammars/rsc install`; `npx --prefix grammars/rsc tree-sitter --version` (0.26.x) |
| 2 | `tree-sitter test` corpus failures (`ERROR`/`MISSING` nodes) | Grammar change or stale expected trees | `make parse FILE=<failing>.rsc`; `python scripts/regenerate_tests.py`; helpers: `scripts/generate_tests.py`, `clean_tests.py` |
| 3 | `make generate-check` fails / `parser.c` stale in CI | `parser.c` not committed after `grammar.js` edit | `make generate && git add grammars/rsc/src/ && git commit` |
| 4 | `rsc-ls` download 404 / `Failed to download rsc-ls` | No GitHub Release asset for current `version` + `triple` | `gh release view v$(grep version Cargo.toml \| head -1)` check `rsc-ls-<triple>`; fallback `cargo build -p rsc-ls --release && export PATH="$PWD/target/release:$PATH"` then restart Zed from that shell |
| 5 | WASM `extension.wasm` stale / `Failed to compile grammar` in Zed | Old `parser.c` or cached extension | `make generate-check`; `rm -rf ~/Library/Application\ Support/Zed/extensions/installed/mikrotik-rsc` then `Install Dev Extension`; Zed's `extension_builder` rebuilds on install — don't `cp` manually |
| 6 | `Windows is not supported for rsc-ls auto-download` | `platform_triple()` in `src/lib.rs` returns `Err` on Windows | Build from source: `cargo build -p rsc-ls --release` and put on PATH; no auto-download on Windows |
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
| Tasks template / active | `languages/rsc/tasks.json` → `.zed/tasks.json` (4 tasks: REST, SSH, Dry-run, Validate) |
| Command table | `data/commands.toml` (header: version, timestamp, sha256) |
| Truth source docs | `llms-full.txt` (version in header), `llms.txt` (index) |
| Extraction / sync | `scripts/extract_commands.py`, `scripts/sync_llms.py` |
| Deploy companion | `scripts/mikrotik-deploy.py` (REST `requests`, SSH `paramiko`, 5MiB cap) |
| Grammar publisher | `scripts/publish_grammar.py` (push + update `extension.toml` rev) |
| Test corpus + helpers | `grammars/rsc/test/corpus/*.txt`, `scripts/{generate,clean,regenerate}_tests.py` |
| Extension manifest / WASM | `extension.toml` (grammar `rev`), `src/lib.rs` (auto-download + PATH fallback, `platform_triple`) |
| LSP binary (native) | `lsp/Cargo.toml`, `lsp/src/main.rs` (stdio JSON-RPC, size caps) |
| LSP modules | `lsp/src/menus.rs` (indices), `server.rs` (caps/URI validation tests), `completion.rs`, `hover.rs`, `diagnostics.rs` (5 rules, capped) |
| Workspace / build | `Cargo.toml` (workspace `lsp`, `wasm32-wasip2`), `Makefile`, `extension.wasm` |
| CI / Release | `.github/workflows/ci.yml`, `release.yml` (4 triples + WASM + GitHub Release) |
