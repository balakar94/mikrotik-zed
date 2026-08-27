# Changelog

## [Unreleased]

No changes yet.

## [0.5.2] - 2026-08-27

### Fixed

- **Extraction (`scripts/extract_commands.py`)**: deduplicate intra-menu arguments (first-wins, 19 removed in `/interface/wifi` family), escape `type` field, optimize `clean_type` truncation 100→150 for complex types, add hygiene trace for empty descriptions.
- **LSP hover (`lsp/src/hover.rs`)**: robust hover for flags with empty upstream descriptions — fallback to `Type: ...` instead of empty card.
- **Data (`data/commands.toml`, `data/upstream-docs.toml`)**: regenerated from upstream `llms-full.txt` (2026-08-27, +253 lines, +8 headings), still 1077 menus.

### Changed

- **Docs (`README.md`, `AGENTS.md`, `extension.toml`)**: clarify version support — `RouterOS 7.20+ · Snapshot 7.23.2 · Awaiting 7.24` (compatible with 7.0+ for common menus).
- **Build (`Cargo.toml`, `lsp/Cargo.toml`)**: pin `toml = "0.8"` comment for MSRV 1.90 clarity.

## [0.5.1] - 2026-08-26

### Added

- **Shim cache integrity**: new `src/cache.rs` introducing versioned binary layout `rsc-ls-<version>` (with `.exe` on Windows) and a `.verified` marker file; reuse path re-hashes the cached binary before execution.
- **Platform helpers**: `platform::stored_binary_name()` and `platform::pinned_release_url()` — single source for cached filename and GitHub Releases URL pinning.
- **Grammar tokens**: `mac_address` and `duration` tokens; `$1` positional parameter; `boolean_literal` and `array_access` precedence fixes in `tree-sitter-rsc`.
- **Corpus coverage**: `line_continuation` corpus case, `tree-sitter test` now 68/68 passing.

### Changed

- **Shim (`src/lib.rs`, `src/platform.rs`, `src/verify.rs`)**: URL pinning for GitHub Releases download; truncated/corrupt file cleanup on verification failure; `WASM shim` still `wasm32-wasip2` clean.
- **Grammar pin (`extension.toml`)**: updated `rev` via `scripts/publish_grammar.py --push`.
- **Highlights (`languages/rsc/highlights.scm`)**: capture corrections — `@keyword.control` -> `@keyword`, `@string.special` -> `@string`, etc. — mirrored to `grammars/rsc/queries/` for corpus `highlight` tests.
- **Data pipeline (`data/commands.toml`, `data/upstream-docs.toml`)**: RouterOS `7.22+` -> `7.23.2`, 1077 menus, source hash updated; `&gt;` entity decoding fix.
- **Scripts (`scripts/sync_llms.py`, `scripts/extract_commands.py`)**: version now read from `data/upstream-docs.toml`; `html.unescape` for entities; atomic writes; exponential backoff for upstream fetch.

### Fixed

- **LSP framing (`lsp/src/framing.rs`)**: bounded header reading with `MAX_HEADER_SIZE`; rejects oversized/malformed `Content-Length` headers.
- **LSP diagnostics (`lsp/src/diagnostics.rs`)**: deferred materialization via `SyntaxFinding` — logical-line reasoning for backslash continuations with physical-line reporting.
- **LSP server (`lsp/src/server.rs`)**: `didChange` batch handling corrected; removed unnecessary clones; duplicate request `id` detection; `invalid_params` error mapping.
- **LSP logging (`lsp/src/logging.rs`, `lsp/src/main.rs`)**: consistent `[rsc-ls][LEVEL]` prefix; removed dead code.

### Chore

- **CI (`.github/workflows/ci.yml`)**: `fetch` before `test-python` so grammar rev check has history; `cargo --locked` for checks; Windows cache key fix.
- **Release (`.github/workflows/release.yml`)**: `cargo --locked` for both WASM and native builds; `save-if` guard on WASM artifact; `concurrency` group to avoid overlapping releases.
- **Security audit**: `cargo audit` gating aligned with `make audit`.
- **Docs drift**: weekly upstream watchdog now surfaces via `upstream-docs` labeled issue.
- **Makefile**: `validate` now includes `commands.toml` diff check.
- **Tests**: staleness suite tolerates absent `llms-full.txt` on clean checkout.

## [0.5.0] - 2026-08-24

Baseline release tagged `v0.5.0`. Changes since `v0.4.0`:

### Added

- **LSP navigation**: go-to-definition and find-references for script variables (`:local`/`:global` <-> `$name`), with document-symbol integration.
- **LSP completion**: `:` trigger for scripting keywords; context-aware menus/verbs/properties/values with snippets and inline docs.
- **LSP quick fixes**: "Did you mean ...?" for invalid enum values via edit distance.
- **LSP folding/symbols**: folding ranges and document outline for menus and variables.
- **E2E harness**: permanent end-to-end tests over real stdio wire, extracted from inline tests.
- **Shim Windows support**: auto-download with `.exe` suffix handling; `aarch64-pc-windows-msvc` and `x86_64-pc-windows-msvc` triples.
- **Highlights**: visible variable highlighting (`$var`, `${var}`) and field-level improvements.

### Changed

- **Grammar enclosure**: `grammars/rsc` converted from git submodule to untracked working copy pinned via `extension.toml` `rev`; `make grammar-clone` and `scripts/publish_grammar.py` updated.
- **Queries**: aligned with modern Zed validation.
- **MSRV / toolchain**: pinned Rust `1.90` + `wasm32-wasip2` (matches `zed-industries/extensions` packaging toolchain).
- **Release**: added `aarch64-pc-windows-msvc` cross-compile target; six platform triples total.
- **README / docs**: user-first rewrite, verified install table, platform list, and deploy docs.

### Fixed

- Grammar: line continuations inside string literals.
- Highlights: capture identification for strings, comment values and `yes`/`no`.
- CI: `cargo audit` invoked via subcommand and pinned.
- Makefile: stale `help` text.

### Chore

- Bump version `0.4.0` -> `0.5.0` (`Cargo.toml`, `lsp/Cargo.toml`, `extension.toml`).
- `extension.toml` kept to schema-known keys only.
- Local `TODO.md` ignored.

[Unreleased]: https://github.com/balakar94/mikrotik-zed/compare/v0.5.2...HEAD
[0.5.2]: https://github.com/balakar94/mikrotik-zed/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/balakar94/mikrotik-zed/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/balakar94/mikrotik-zed/compare/v0.4.0...v0.5.0
