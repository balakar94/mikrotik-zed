# Changelog

## [Unreleased]

No changes yet.

## [0.5.2] - 2026-08-27

### Fixed

- **Extraction (`scripts/extract_commands.py`)**: deduplicate intra-menu arguments (first-wins, 19 removed in `/interface/wifi` family), escape `type` field via `escape_toml_string`, optimize `clean_type` truncation 100→150 for complex types, add hygiene trace for empty descriptions (77% args).
- **LSP hover (`lsp/src/hover.rs`)**: robust hover for flags with empty upstream descriptions — fallback to `Type: ...` instead of empty card.
- **Data (`data/commands.toml`, `data/upstream-docs.toml`)**: regenerated from upstream `llms-full.txt` hash `5503bd5858cf82d0` (2026-08-27, +253 lines, +8 headings), still 1077 menus, source hash updated.

### Changed

- **Docs (`README.md`, `AGENTS.md`, `extension.toml`)**: clarify version support — `RouterOS 7.20+ · Snapshot 7.23.2 · Awaiting 7.24` (compatible with 7.0+ for common menus).
- **Build (`Cargo.toml`, `lsp/Cargo.toml`)**: pin `toml = "0.8"` comment for MSRV 1.90 clarity.

## [0.5.1] - 2026-08-26

### Added

- **Shim cache integrity (Phase 1 — P0.1/P0.2, `0cede40`)**: new `src/cache.rs` (305 lines) introducing versioned binary layout `rsc-ls-<version>` (with `.exe` on Windows) and a `.verified` marker file; reuse path re-hashes the cached binary before execution.
- **Platform helpers**: `platform::stored_binary_name()` and `platform::pinned_release_url()` — single source for cached filename and GitHub Releases URL pinning.
- **Grammar tokens (Phase 3, `fdba9fa`)**: `mac_address` and `duration` tokens; `$1` positional parameter; `boolean_literal` and `array_access` precedence fixes in `tree-sitter-rsc` (`grammars/rsc/grammar.js`).
- **Corpus coverage**: `line_continuation` corpus case, `tree-sitter test` now 68/68 passing.

### Changed

- **Shim (`src/lib.rs`, `src/platform.rs`, `src/verify.rs`, `0cede40`)**: URL pinning for GitHub Releases download; truncated/corrupt file cleanup on verification failure; `WASM shim` still `wasm32-wasip2` clean (`zed_extension_api::current_platform()` only, no `std::env::var`).
- **Grammar pin (`extension.toml`, `fdba9fa`)**: `rev` `1f79a22` -> `2fdfe88` (`2fdfe888ab37d15a2bc4265e0d4bb12193b56f5f`), published via `scripts/publish_grammar.py --push`.
- **Highlights (`languages/rsc/highlights.scm`, `fdba9fa`)**: capture corrections — `@keyword.control` -> `@keyword`, `@string.special` -> `@string`, etc. — mirrored to `grammars/rsc/queries/` for corpus `highlight` tests.
- **Data pipeline (`data/commands.toml`, `data/upstream-docs.toml`, `87cc692`)**: RouterOS `7.22+` -> `7.23.2`, 1077 menus, source hash `44043d3ad9d4a6eb` -> `df1882575dc393fa` (2026-08-26T17:10:20Z); `&gt;` entity decoding fix.
- **Scripts (`scripts/sync_llms.py`, `scripts/extract_commands.py`, `87cc692`)**: version now read from `data/upstream-docs.toml`; `html.unescape` for entities; atomic writes (`tmp` + `rename`); exponential backoff for upstream fetch.

### Fixed

- **LSP framing (`lsp/src/framing.rs`, `6e18b86`)**: bounded header reading with `MAX_HEADER_SIZE`; rejects oversized/malformed `Content-Length` headers instead of unbounded allocation.
- **LSP diagnostics (`lsp/src/diagnostics.rs`, `6e18b86`)**: deferred materialization via `SyntaxFinding` — logical-line reasoning for backslash continuations with physical-line reporting; syntax-limit diagnostics no longer eagerly rendered.
- **LSP server (`lsp/src/server.rs`, `6e18b86`)**: `didChange` batch handling corrected; removed unnecessary clones; duplicate request `id` detection; `invalid_params` error mapping; `eprintln!` -> `log` migration; `log_trace` cleanup. Verified with 544 `rsc-ls` + e2e tests.
- **LSP logging (`lsp/src/logging.rs`, `lsp/src/main.rs`, `6e18b86`)**: consistent `[rsc-ls][LEVEL]` prefix; removed dead code in `main.rs`.

### Chore

- **CI (`.github/workflows/ci.yml`, `87cc692`)**: `fetch` before `test-python` so grammar rev check has history; `cargo --locked` for `check-wasm`/`check-lsp`; Windows `Swatinem/rust-cache` key fix; `pip install` without `--require-hashes` plain args.
- **Release (`.github/workflows/release.yml`, `87cc692`)**: `cargo --locked` for both WASM and native builds; `cargo-zigbuild` pinned `0.17.3`; `save-if` guard on WASM artifact; `concurrency` group to avoid overlapping releases.
- **Security audit (`.github/workflows/security-audit.yml`, `87cc692`)**: `cargo audit` gating aligned with `make audit`.
- **Docs drift (`.github/workflows/docs-drift.yml`, `87cc692`)**: weekly upstream watchdog now surfaces via `upstream-docs` labeled issue.
- **Makefile (`Makefile`, `87cc692`)**: `validate` now includes `commands.toml` diff check (`make extract` idempotency gate).
- **Tests (`tests/`)**: staleness suite tolerates absent `llms-full.txt` on clean checkout (from `2cc591e` base, carried through).

## [0.5.0] - 2026-08-24

Baseline release tagged `v0.5.0` (`e167430`). Changes since `v0.4.0` (`62998ab`):

### Added

- **LSP navigation (`lsp/src/navigation.rs`)**: go-to-definition and find-references for script variables (`:local`/`:global` <-> `$name`), with document-symbol integration.
- **LSP completion (`lsp/src/completion.rs`)**: `:` trigger for scripting keywords; context-aware menus/verbs/properties/values with snippets and inline docs.
- **LSP quick fixes (`lsp/src/diagnostics.rs`)**: "Did you mean ...?" for invalid enum values via edit distance.
- **LSP folding/symbols**: folding ranges and document outline for menus and variables.
- **E2E harness (`lsp/tests/e2e.rs`, `lsp/src/main.rs`)**: permanent end-to-end tests over real stdio wire (986 lines), extracted from inline tests (`lsp/src/main_tests/`).
- **Shim Windows support (`src/lib.rs`, `src/platform.rs`)**: auto-download with `.exe` suffix handling; `aarch64-pc-windows-msvc` and `x86_64-pc-windows-msvc` triples.
- **Highlights (`languages/rsc/highlights.scm`)**: visible variable highlighting (`$var`, `${var}`) and field-level improvements.

### Changed

- **Grammar enclosure**: `grammars/rsc` converted from git submodule to untracked working copy pinned via `extension.toml` `rev` (`40ca7ab` -> `1f79a22`); `make grammar-clone` and `scripts/publish_grammar.py` updated; packaging no longer hits "grammar directory already exists, but is not a git clone" in `zed-industries/extensions`.
- **Queries (`languages/rsc/indents.scm`, `injections.scm`)**: aligned with modern Zed validation.
- **MSRV / toolchain (`rust-toolchain.toml`)**: pinned Rust `1.90` + `wasm32-wasip2` (matches `zed-industries/extensions` packaging toolchain).
- **Release (`release.yml`)**: added `aarch64-pc-windows-msvc` (Windows ARM64) cross-compile target; six platform triples total.
- **README / docs**: user-first rewrite, verified install table, platform list, and deploy docs.

### Fixed

- Grammar: line continuations inside string literals (`#26`).
- Highlights: capture identification for strings, comment values and `yes`/`no` (`#25`).
- CI: `cargo audit` invoked via subcommand and pinned.
- Makefile: stale `help` text.

### Chore

- Bump version `0.4.0` -> `0.5.0` (`Cargo.toml`, `lsp/Cargo.toml`, `extension.toml`).
- `extension.toml` kept to schema-known keys only (`make check-manifest` gate).
- Local `TODO.md` ignored.

[Unreleased]: https://github.com/balakar94/mikrotik-zed/compare/v0.5.2...HEAD
[0.5.2]: https://github.com/balakar94/mikrotik-zed/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/balakar94/mikrotik-zed/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/balakar94/mikrotik-zed/compare/v0.4.0...v0.5.0
