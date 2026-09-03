# Skill: QA, CI & Release

## When to Use

Load this skill when the task involves any of: `test`, `pytest`, `cargo test`, `clippy`, `fmt`, `ci.yml`, `release.yml`, `validate`, `sync-check`, `docs-drift`, QA gates, or release hygiene. For day-to-day build commands and manual Zed iteration see `development-workflow.md` — this skill covers gates, not daily loops.

Hard constraints in [`AGENTS.md`](../../AGENTS.md) → *Hard rules* still apply (no `zed` clone, no bundled `rsc-ls`, `wasm32-wasip2` only, edit inputs not generated outputs, `extension.toml` schema-only, caps/defensive LSP, English-only).

## Testing Strategy

| Level | Location | Command | What it Guards |
|-------|----------|---------|----------------|
| Unit (Rust) | `lsp/src/` | `cargo test -p rsc-ls` (603 tests) | `menus`, `parser`, `completion`, `hover`, `diagnostics` (7 rules, `MAX_DIAG_LINES` 3000 / `500KB`), `symbols`, `folding`, `framing`, `encoding`, `caps`, `live` cache/SSRF/host caps |
| E2E wire | `lsp/tests/e2e.rs` (14 tests) | `cargo test -p rsc-ls --test e2e` | Real binary via `CARGO_BIN_EXE_rsc-ls`, Content-Length framing, incremental `change=2` sync, `publishDiagnostics` (incl. split-URL continuation), `completion`/`hover`/`documentSymbol`/`foldingRange`/`codeAction`/`signatureHelp`, variable `definition`/`references`, `-32601` echo, shutdown→exit 0, UTF-16 default |
| Python | `tests/` (346 tests) | `pytest tests/ -v` / `make test-python` | Extraction (`test_extract_commands.py`), coverage (`test_commands_coverage.py`), enclosure/caps (`test_enclosure.py`), env/live opt-in (`test_live_opt_in.py`, `RSC_LS_LIVE`/`MIKROTIK_LIVE` in-memory only), staleness (`test_docs_staleness.py`), `extension.toml` schema (`test_zed_requirements.py`), release coherence (`test_release.py`) |
| Grammar corpus | `grammars/rsc/test/corpus/*.txt` | `npx tree-sitter test` / `make test-grammar` | Node types, `ERROR`/`MISSING` regressions; update expectations with `npx tree-sitter test -u` (native) |

Harness notes: e2e reader thread + `mpsc`, every wait bounded at 5s (`RECV_TIMEOUT`) so a wedged server fails fast not hangs. Live tests are opt-in (`MIKROTIK_HOST`/`PASS`) — skipped otherwise. `validate` embedding is `include_str!()` → rebuild `rsc-ls` after `data/commands.toml` changes.

Suite aliases (see `make help` canonical):

```bash
make test-grammar   # tree-sitter corpus only
make test-rust      # cargo test --workspace (603 + 14 e2e)
make test-python    # pytest 346
make test-all       # all three sequentially
```

### Harness Invariants & Data Prerequisites

- E2E spawns the real `rsc-ls` binary; std-only client (threads + `mpsc` + `serde_json`), no extra deps. `MAX_HEADER_SIZE` 32 KiB / `MAX_BODY_BYTES` 64 MiB sanity caps mirror `lsp/src/caps.rs`.
- Diagnostics fixtures include the hagezi split-URL continuation (`\` + quoted URL spanning lines) — must not emit `unknown-menu`/`unclosed-quote` false positives; incremental `didChange` range edit must republish without the fixed typo.
- Python `tests/` requires `pytest` + `tomli` (bootstrap via `make install-tools` → `.venv`); live enrichment tests skip unless `RSC_LS_LIVE=1`/`MIKROTIK_HOST` set, and never log `MIKROTIK_PASS`.
- Grammar corpus expects `parser.c`/`grammar.json`/`node-types.json` generated; `make generate-check` enforces committed state.

## CI Gates (`.github/workflows/ci.yml`)

Triggers: `push`/`pull_request` on `main`. Concurrency `ci-${{ github.ref }}` cancels superseded runs. MSRV Rust 1.90 + `wasm32-wasip2` + Python 3.12 + Node 24.

| Job | Key Steps | Failure Signal |
|-----|-----------|----------------|
| `rust` (`ubuntu-latest`) | `make fmt` (`cargo fmt -- --check`), `make clippy` (`wasm32-wasip2` + `rsc-ls --all-targets -D warnings`), `make test-rust`, `cargo build --target wasm32-wasip2 --release`, `cargo build -p rsc-ls --release` | `fmt`/`clippy` must be clean; WASM+LSP compile gate |
| `windows` (`windows-latest`) | `cargo test -p rsc-ls --locked`, `cargo build -p rsc-ls`, cross-check `aarch64-pc-windows-msvc` | Windows MSVC linkage regressions |
| `python` (`ubuntu-latest`) | `make check-manifest` (schema via `scripts/check_zed_requirements.py`), `make sync` fetch `llms.txt`/`llms-full.txt`, `make test-python`, verify `data/commands.toml` timestamp-agnostic (`grep -v '^# Generated:'` diff vs `HEAD`, exit 1 if stale) | Unknown `extension.toml` keys (silently ignored by Zed), stale extraction |
| `grammar` (`ubuntu-latest`) | Clone at pinned `extension.toml` `rev`, `make generate-check` (`npx tree-sitter generate` + `git diff --exit-code src/parser.c src/grammar.json src/node-types.json`), `make test-grammar`, `rev` vs `HEAD` coherence | Stale `parser.c`, placeholder `000...` rev, corpus failure |

`sync-check` is NOT in `ci.yml` `validate` — it is a separate staleness gate: `python scripts/sync_llms.py --check` exits `0` ok / `2` drift / `1` fetch error. CI surfaces drift via `verify commands.toml` step (timestamp-agnostic).

## Docs Drift (`.github/workflows/docs-drift.yml`)

Weekly watchdog: `schedule: 43 5 * * 1` (Mon 05:43 UTC) + `workflow_dispatch`. Permissions `contents:read`, `issues:write`.

- Runs `python scripts/sync_llms.py --check` → maps `0→ok`, `2→drift`, `*→error`.
- `drift` → label-bootstrap `upstream-docs` (`D93F0B`), open or comment on single issue `Upstream RouterOS docs drifted…` with UTC date + `check.out` + fix `python scripts/sync_llms.py && python scripts/extract_commands.py`.
- `ok` → auto-close open `upstream-docs` issue (`Resolved` / `completed`).
- `error` → `::warning` only (transient network stays green). Concurrency `docs-drift`, no dedup window.

## Release Gates (`.github/workflows/release.yml`)

Triggers: `push` tags `v*.*.*` or `workflow_dispatch` (optional `tag` input, falls back to `Cargo.toml` version). Concurrency `release-${{ github.ref }}` (no cancel).

| Job | Runner | Output |
|-----|--------|--------|
| `meta` | `ubuntu-latest` | Resolves `tag`/`version`, validates `vMAJOR.MINOR.PATCH`, asserts `Cargo.toml` + `lsp/Cargo.toml` == `tag` (fail), `extension.toml` == `tag` (warn), `workflow_dispatch` guard (tag exists + `git rev-parse "$TAG^{commit}"` == `github.sha`), sets `RSC_LS_BUILD_SHA=${{ github.sha }}` |
| `preflight` (`needs: [meta]`) | `ubuntu-latest` | On the release SHA: `cargo test --workspace --locked`, `cargo clippy -p rsc-ls --all-targets -- -D warnings`, `make check-manifest`, `make generate-check` (grammar clone + Node 24, same recipe as `ci.yml` grammar job) |
| `wasm` (`needs: [meta, preflight]`) | `ubuntu-latest` | `cargo build --target wasm32-wasip2 --release` → `extension.wasm` + `extension.wasm.sha256` + `SHA256SUMS` (`sha256sum` with `shasum` fallback), attest provenance, upload 14d |
| `build-*` (6 explicit jobs, `needs: [meta, preflight]`) | `ubuntu-latest`/`ubuntu-24.04-arm`/`macos-latest`/`windows-latest` | `cargo build -p rsc-ls --target <triple>` native on `ubuntu-24.04-arm` for `aarch64-unknown-linux-gnu` (no `zig`/`cargo-zigbuild` cross), 6 binaries: `rsc-ls-{aarch64,x86_64}-{apple-darwin,unknown-linux-gnu,pc-windows-msvc}` + per-file `.sha256` + `SHA256SUMS`, smoke `<binary> --version` (Rosetta-guarded for `x86_64-apple-darwin`, ARM64 PE `file` check for `aarch64-pc-windows-msvc`), attest + upload |
| `release` | `ubuntu-latest` | Guards idempotency (`gh release view`), merges `dist/*`, rebuilds combined `SHA256SUMS`, generates notes, `softprops/action-gh-release` (`fail_on_unmatched_files: true`) → GH Release with 6 `rsc-ls-<triple>` + `extension.wasm` + `*.sha256` + `SHA256SUMS` |
| `validations` | `ubuntu-latest` | Re-validates `extension.toml` via `check_zed_requirements.py`, placeholder `000...` guard |
| `postflight` (`needs: [meta, release]`) | `ubuntu-latest` | Downloads every asset + its `.sha256` companion from the live release (`GITHUB_TOKEN` suffices) and `sha256sum -c` each plus combined `SHA256SUMS` |

Download verification on consumer side is `sha256sum -c SHA256SUMS` (WASM shim does built-in SHA-256 before exec).

## Local Validation

Use `make help` as canonical list. Two gates matter for QA:

```bash
make check      # fast compile gate: check-wasm + check-lsp (no tests)
make validate   # full pre-commit/pre-PR gate: check-manifest + generate-check + fmt + clippy + test-all + extract
                # includes extract idempotency: `git diff --exit-code data/commands.toml` (timestamp ignored in CI only)
make sync-check # standalone staleness gate: exits 2 on drift, 1 on fetch error — NOT part of validate
```

Manual e2e debug (wire harness stderr is nulled by default):

```bash
RSC_LS_E2E_STDERR=1 cargo test -p rsc-ls --test e2e -- --nocapture
cargo test -p rsc-ls -- --nocapture          # unit + e2e with logs (RSC_LS_LOG=debug for server logs)
```

After `data/commands.toml` regeneration: `cargo test -p rsc-ls` must rebuild (embedded via `include_str!()`). After `grammars/rsc/grammar.js` edit: `make generate && make test-grammar` before `validate`.

## Pre-commit / Pre-PR Checklist

> Enforced by `make validate` + `docs-drift`/`sync-check` + `release.yml` `meta`.

- [ ] **CHANGELOG.md**: move `Unreleased` → `## [x.y.z] - YYYY-MM-DD` (`Fixed`/`Changed`/`Added`), fix footnotes (`[Unreleased]: compare/vx.y.z...HEAD`, `[x.y.z]: compare/v...v...`).
- [ ] **ROADMAP.md**: update `Now — 0.5.x` tag/hash snapshot; keep `Volatile facts` pointer, no redundant principles.
- [ ] **No hardcoded versions**: bump via `make bump VERSION=x.y.z` (syncs `Cargo.toml`/`lsp/Cargo.toml`/`extension.toml` + `cargo fmt`); grammar crate versions independent.
- [ ] **Caps & Hard rules** (`AGENTS.md` §7): `MAX_MESSAGE_SIZE` 10 MiB / `MAX_HEADER_SIZE` 32 KiB / `MAX_DOC_SIZE` 5 MiB / `MAX_DOCS` 100 / `MAX_DIAG_LINES` 3000 / `500KB` / live TTL 60s / negative 15s / `LIVE_MAX_HOSTS` 4 / `LIVE_CUSTOM_RESOURCES_MAX` 8 / SSRF `169.254.169.254` / timeouts 5s/2s (1..30s clamp) — no filesystem beyond cache, live opt-in only, strict `file://` URI.
- [ ] **`make validate` green** (fixes `fmt` via `make fmt-fix` before re-check).
- [ ] **If `llms-full.txt` touched**: `make extract` + `head -20 data/commands.toml` + `cat data/upstream-docs.toml` hash/version, spot-check `rg -c '^\[\[menus\]\]'`.
- [ ] **If `grammar.js` touched**: `publish_grammar.py --push` updates `extension.toml` `rev` — never hand-edit `rev`.

## Troubleshooting

| # | CI Failure | Cause | Fix |
|---|------------|-------|-----|
| 1 | `generate-check` / `parser.c stale` | `grammars/rsc/grammar.js` changed without regenerate | `make generate && git add grammars/rsc/src/ && git commit` |
| 2 | `sync-check` exit 2 / `data/commands.toml stale` | `llms-full.txt` updated but `data/commands.toml` not re-extracted | `make sync && make extract && git add data/commands.toml data/upstream-docs.toml` |
| 3 | `clippy` `-D warnings` (wasm + lsp) | Lint regression in `src/lib.rs` (wasm) or `lsp/src/` | `cargo clippy --target wasm32-wasip2 -- -D warnings`; `cargo clippy -p rsc-ls --all-targets -- -D warnings` |
| 4 | `fmt` check | Unformatted Rust | `make fmt-fix && git add -u` |
| 5 | `check-manifest` unknown keys | `extension.toml` has schema-unknown key (silently ignored by Zed) | `python scripts/check_zed_requirements.py`; remove/rename key; see `zed-extension-dev.md` |
| 6 | `test-rust` / `test-python` red | Unit or extraction regression (603 Rust / 346 Python / 14 e2e) | `cargo test -p rsc-ls -- --nocapture`; `pytest tests/ -v`; `RSC_LS_E2E_STDERR=1 cargo test --test e2e -- --nocapture` |
| 7 | `test-grammar` corpus failure (`ERROR`/`MISSING`) | Grammar regression or stale expectations | `make parse FILE=...` then `cd grammars/rsc && npx tree-sitter test -u`, review diff |
| 8 | `docs-drift` issue opened (`upstream-docs`) | Upstream RouterOS docs drifted from `data/upstream-docs.toml` snapshot | Follow issue body: `python scripts/sync_llms.py && python scripts/extract_commands.py`, review `git diff data/`, commit & push → watchdog auto-closes |
| 9 | `release.yml` `meta` version mismatch | `Cargo.toml`/`lsp/Cargo.toml` != tag `v*.*.*` | `make bump VERSION=x.y.z && cargo check && git diff` then `git tag vX.Y.Z && git push origin vX.Y.Z` |
| 10 | `release.yml` placeholder `rev` | `extension.toml` `rev = "000..."` | `python scripts/publish_grammar.py --push` then re-tag |

Also: `make audit` needs `cargo install cargo-audit`; `extension.wasm` is gitignored — built by `release.yml`/`make build-wasm`.

## Risk & Gate Mapping

Focus gates on highest-risk surfaces: data integrity (extraction → `data/commands.toml` header, 15-verb coverage), protocol fidelity (Content-Length framing, incremental sync, UTF-16), defensive limits (caps, SSRF deny, timeout clamps), and release determinism (tag vs `Cargo.toml`/`extension.toml` coherence).

| Risk | Gate | Signal |
|------|------|--------|
| Stale command table | `python` job + `docs-drift` issue | `grep -v '^# Generated:'` diff fails; `upstream-docs` label |
| Grammar desync | `grammar` job `generate-check` + `rev` coherence | `parser.c` diff / `000...` placeholder / corpus `ERROR` |
| WASM vs LSP drift | `rust` job dual `clippy`/`build` | `wasm32-wasip2` vs native lint/build divergence |
| Release mis-tag | `release.yml:meta` | `Cargo.toml` version != `v*.*.*` (fail-closed) |

## Flaky-Test & Timeout Diagnostics

- E2E flakes: check 5s `RECV_TIMEOUT` panics — increase only if cold debug build on loaded CI; prefer `RSC_LS_E2E_STDERR=1` to see server stderr vs bumping timeout.
- Live timeout clamps (1..30s `MIKROTIK_TIMEOUT`, blocking 2s / per-request 5s): validate with `scripts/mikrotik-live-check.py --dry-run --json` before enabling enrichment.
- Python `pytest` flakes from network: `sync_llms.py --check` exit 1 is warning-only; re-run `make sync` then `make test-python` with local `llms-full.txt` present.
- Grammar `npx tree-sitter test -u` rewrites expectations — always `git diff` corpus files; never commit without reviewing `ERROR`/`MISSING` removals.

## References

- Day-to-day commands, grammar edits, extraction loops: `development-workflow.md`.
- LSP feature details, caps source-of-truth (`lsp/src/caps.rs`): `language-server.md`.
- Manifest schema & publishing: `zed-extension-dev.md`; upstream truth `llms-full.txt` → `data/commands.toml`: `commands-extraction.md`.
- Hard rules, volatile facts, repo map: [`AGENTS.md`](../../AGENTS.md) — always re-check `extension.toml` `rev`, `make help`, `data/commands.toml` header before quoting versions.
