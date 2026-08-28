# Changelog

## [Unreleased]

## [0.5.3] - 2026-08-28

### Added

- **Live Device Data Enrichment (`lsp/src/live.rs`, `lsp/src/completion.rs`)**:
  - Opt-in live RouterOS data enrichment for LSP autocompletion over REST (`RSC_LS_LIVE=1` or `MIKROTIK_LIVE=1`).
  - **Generic Live Resource Dispatcher (`ResourceKind`)** supporting:
    - **Interfaces & Bridges**: `interface`, `bridge`, `in-interface`, `out-interface`, `parent`, and all `iface`-typed properties.
    - **IP Addresses & Networks**: `address`, `network`, `src-address`, `dst-address`, `gateway`, `to-addresses`, with IPv4, IPv6, and CIDR prefix sanitization.
    - **Firewall & Lists**: `src-address-list`, `dst-address-list`, `address-list`, `list`, `chain`, `jump-target` across Filter, NAT, Mangle, and Raw.
    - **IP Pools**: `pool`, `address-pool`, `pool-name`, `remote-pool` (IPv4 and IPv6).
  - In-memory `LiveCache` (TTL 60s, max 16 collections, max 500 items, max 64 chars per value, max 512 KiB response payload) with LRU eviction and zero disk persistence.
  - Bounded 2-second blocking fetch budget with silent honest fallback when router is offline or unreachable.
  - Strict host validation rejecting control characters and URI delimiters; passwords strictly redacted from all debug logs and errors.
  - Interactive Zed tasks in `languages/rsc/tasks.json` for live connectivity checks.
  - Comprehensive QA coverage: 46 Python tests in `tests/test_live_opt_in.py` and dedicated Rust unit tests.
- **Live Hardening — Enriched Connection System**:
  - Non-blocking hydrator with coalescing: `textDocument/completion` no longer blocks the LSP loop (stale-while-revalidate via background thread, 2s coalescing per `ResourceKind`).
  - Negative cache / circuit breaker: failed fetches enter 15s cooldown (`LIVE_NEGATIVE_TTL_SECS`) to prevent retry spam when router is offline.
  - TLS `MIKROTIK_SSL=0` now actually disables rustls verification via custom `ServerCertVerifier` + `OnceLock` agent cache (previously only logged).
  - Robust URL building with `url` crate, IPv6 bracket handling (`fe80::1` → `[fe80::1]`), and SSRF denial for `169.254.169.254` / `metadata.google.internal`.
  - Multi-host support: `MIKROTIK_HOST="a,b,c"` comma-split, capped `LIVE_MAX_HOSTS=4`, validated per-host.
  - Generic dispatcher extensibility via `RSC_LS_LIVE_RESOURCES='[{"property":"packet-mark","path":"/rest/...","field":"new-packet-mark"}]'` (capped `LIVE_CUSTOM_RESOURCES_MAX=8`).
  - Workspace commands `rsc.live.refresh` / `rsc.live.status` (`executeCommandProvider`) and hot-reload via `workspace/didChangeConfiguration` (no Zed restart).
  - Observability: `OnceLock` agent reuse, structured `live fetch ok` logs with `latency_ms` / `items`, `ssl_verify_effective` in startup banner.
  - Real health check: new `scripts/mikrotik-live-check.py` (GET `/rest/interface` with Basic Auth, mirrors `live.rs` scheme/host validation, `--dry-run`/`--json`, never logs `pass`) and updated `languages/rsc/tasks.json` + `.zed/tasks.json` (6 tasks, 2 live, identical).

### Fixed

- **LSP Live (`lsp/src/live.rs`)**: `MIKROTIK_SSL=0` was a no-op (only `debug!`); now `warn!` + real insecure verifier. SSRF hosts rejected, bare IPv6 literals correctly bracketed.

### Changed

- **Tasks (`languages/rsc/tasks.json`, `.zed/tasks.json`)**: `Live — Check connectivity` now runs real `mikrotik-live-check.py` (not `deploy --dry-run`), shares env semantics with `live.rs` (`PORT 443`, `TIMEOUT 5s clamped 1..30`, `MIKROTIK_HTTP`/`SSL` scheme logic).
- **Deploy (`scripts/mikrotik-deploy.py`)**: header notes env vars are mirrored in `lsp/src/live.rs LiveConfig::from_env`; no behavior change.
- **Caps (`lsp/src/caps.rs`)**: added `LIVE_NEGATIVE_TTL_SECS=15`, `LIVE_MAX_HOSTS=4`, `LIVE_CUSTOM_RESOURCES_MAX=8`.

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

[Unreleased]: https://github.com/balakar94/mikrotik-zed/compare/v0.5.3...HEAD
[0.5.3]: https://github.com/balakar94/mikrotik-zed/compare/v0.5.2...v0.5.3
[0.5.2]: https://github.com/balakar94/mikrotik-zed/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/balakar94/mikrotik-zed/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/balakar94/mikrotik-zed/compare/v0.4.0...v0.5.0
