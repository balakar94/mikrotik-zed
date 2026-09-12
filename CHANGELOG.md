# Changelog

## [Unreleased]

### Security

- **LSP input boundaries (`lsp/src/server_proto.rs`, `lsp/src/live_config.rs`)**: malformed JSON-RPC with a multi-byte character after `"id"` and a workspace-settings fingerprint whose `sha256:` prefix boundary falls inside a multi-byte character no longer abort the server.
- **Live TLS (`lsp/src/live_fetch.rs`)**: the SPKI pin verifier now verifies the TLS handshake signature against the pinned leaf instead of asserting it, closing a certificate-replay MITM against `MIKROTIK_FINGERPRINT`.
- **Live SSRF (`lsp/src/live_net.rs`, `scripts/_mikrotik_shared.py`)**: trailing-dot hostnames, NAT64/Teredo/6to4 and IPv4 special-use ranges (multicast, reserved, `192.0.0.0/24`, benchmarking) are denied; ULA/CGNAT are loopback-gated; validated DNS addresses are pinned for Rust and both Python transports (no resolve-then-connect TOCTOU); `MIKROTIK_FINGERPRINT` is verified on the request TLS connection before credentials are sent.
- **Deploy transport (`scripts/mikrotik-deploy.py`, `scripts/mikrotik-live-check.py`)**: device response bodies are redacted and size-capped; the workspace-settings `port` requires the transport opt-in.
- **WASM shim (`src/cache.rs`, `src/platform.rs`, `src/lib.rs`)**: bounded cached-binary and `.verified` marker reads; release download URLs pinned to repo/tag/asset.

### Fixed

- **Diagnostics DoS bounds (`lsp/src/diagnostics.rs`, `lsp/src/suggest.rs`)**: suggestion input cap plus a length short-circuit, a bounded diagnostic message length, and bounded syntax-finding memory during the document walk.
- **Menu handling (`lsp/src/diagnostics.rs`, `lsp/src/completion.rs`, `lsp/src/parser.rs`)**: case-insensitive menu/property/verb lookups, slash canonicalisation, unknown-menu ranges with repeated slashes, partial menu-path completion, and UTF-8 `file://` URI decoding.
- **Data pipeline (`scripts/extract_commands.py`)**: markdown property tables and multi-line `<ArgTableRow>` rows; page-context association; generation fails on non-canonical menu paths.

### Added

- **Grammar value parsing (`grammars/rsc/grammar.js`)**: URL values, colon-containing scalars, block-valued parameters and quoted variable references — real-export parse rate 11/30 → 29/30 clean.
- **Syntax highlighting (`languages/rsc/highlights.scm`)**: `(url)` and `(mixed_value)` captures; grammar pinned at `1e61ad4`.

### Changed

- **Behavior**: completion on an unknown or partial menu no longer advertises the 15 standard verbs; workspace-settings `port` and fingerprint overlays are gated; `MIKROTIK_FINGERPRINT` without `MIKROTIK_CA_FILE` is pure SPKI pinning (self-signed devices work) and environment proxies are ignored by the Python scripts; `file://` URIs containing backslashes are rejected.

## [0.5.6] - 2026-09-10

### Added

- **LSP completion (`lsp/src/completion.rs`)**: deterministic relevance ranking — `sortText` tiers (`0!live_` device truth < required < optional < verb < submenu < enum < common-hint < placeholder < flag < typo-fallback < snippet) with exact/prefix/substring quality inside each tier; truncation at `MAX_COMPLETION_ITEMS=200` is relevance-ordered. Typed-prefix `filterText` plus `textEdit` replacement shadows for values and submenus (`chain=in` + `input` no longer yields `ininput`). Curated `chain=input|forward|output` common hints (`common value — verify on device`); device-dependent types stay silent without live data.
- **LSP hover/signature (`lsp/src/hover.rs`, `lsp/src/signature.rs`, `lsp/src/text_util.rs`)**: shared text helpers (markdown/label/glossary previously triplicated). Menu hover capped at 12 required-first properties with a `(+N more)` footer and required badges; property hover gains context (`in \`/path verb\``) and example lines; 15-verb glossary and 15 colon builtins. Signature filters already-typed pairs, collapses long enums, advances `activeParameter`, and names the verb role. Upstream markdown sanitized, label/detail budgets enforced with offsets intact.
- **LSP diagnostics (`lsp/src/diagnostics.rs`, `lsp/src/menus.rs`, `lsp/src/suggest.rs`)**: Hint-only typed validators (`invalid-bool/num/time/mac/ip/ubit-value`), silent on empty/truncated/dynamic values; `non-unsettable-property` hint (consumes `ArgEntry.unset`) and `read-only-write` notice. Per-publish suggestion budget (100): large unknown-heavy documents publish in ~1.5s instead of stalling past 10s, same codes/severities minus the suffix.
- **LSP structure (`lsp/src/symbols.rs`, `lsp/src/navigation.rs`, `lsp/src/folding.rs`)**: collapsed `(×N)` outline symbols with `comment=`/distinguishing-prop detail; `$var` indexed inside double-quoted strings; folding/symbols caps enforced and sorted.
- **Live TLS (`lsp/src/live.rs`, `scripts/`)**: `MIKROTIK_FINGERPRINT=sha256:<hex>` / `MIKROTIK_CA_FILE` (+ `--fingerprint`/`--ca-file` flags) with fail-closed pin verification; resolve-then-revalidate DNS on every fetch; `/rest/` path boundary; bounded CA loading (256 KiB + negative cache).
- **Tests (`lsp/src/tests/`, `lsp/tests/`)**: white-box suite normalized to `<module>_<aspect>.rs` (96 files, ≤300 soft cap, shared fixtures); new framing-chaos and perf-smoke E2E targets; tasks mirror gate; weekly release perf CI (`.github/workflows/perf.yml`).
- **Zed tasks (`languages/rsc/tasks.json`, `.zed/tasks.json`)**: workflow order check → deploy → verify; Validate retargeted to a real local preflight; no secrets in task env.

### Changed

- **LSP diagnostics (`lsp/src/diagnostics.rs`, `lsp/src/caps.rs`)**: `MAX_DIAGNOSTICS=2000` bound on total semantic diagnostics per publish — `compute_diagnostics` now truncates before the syntax extend, and the `truncated` hint covers the count-only case. Previously a single logical line carrying tens of thousands of distinct unknown keys yielded one heap `Diagnostic` per key.
- **Data (`data/commands.toml`, `data/upstream-docs.toml`)**: `upstream c77198 → c043cd8f` — `nd-ping` description enrichment for `/tool/ping` ("Use IPv6 Neighbor Discovery (NS/NA) instead of ICMP echo to discover hosts"), `1077 menus` stable `7.23.2`.
- **Toolchain (`rust-toolchain.toml`)**: fixed stale comment — workflows resolve `toolchain.channel` dynamically from this file (single source of truth); they never pinned `1.90` explicitly.
- **Caps registry (`lsp/src/caps.rs`)**: indexed `MAX_CONCURRENT_FETCHES=2` and `MAX_DIAGNOSTICS=2000` in the central table, per the "every limit discoverable from ONE place" policy.
- **LSP internals**: removed dead `build_base_url` wrapper (all callers use `build_base_url_with_allow`), unused test helper `cfg_with_no_loopback`, and write-only `LineContext.last_token` field; fixed stale "Uses `build_base_url`" doc references.
- **Docs**: grammar corpus `Simple array` slow-parse warning documented as expected CLI timing noise (`.agents/skills/qa-ci-release.md`); README credential sections now state settings-provided secrets are ignored with a warning.
- **Diagnostics severity (`lsp/src/diagnostics.rs`)**: `missing-required` Information → Warning and `invalid-enum-value` Hint → Warning (both break `/import`); `Did you mean …?` appended to typo messages; syntax cap gets an explicit `truncated` footer.
- **Settings trust (`lsp/src/live.rs`, `lsp/src/server.rs`)**: transport-security keys and settings `host`/`user` redirects are ignored by default unless `RSC_LS_ALLOW_SETTINGS_TRANSPORT=1` (env always wins), with loud WARNs. **Behavior change**: live targets configured only via workspace settings stop applying; set the host via env or opt in explicitly.
- **Legacy HTTP shim (`lsp/src/live.rs`)**: silent `port 80 + SSL=0 → http` downgrade is off by default (matches the Python companions); opt back in with `RSC_LS_LEGACY_HTTP_SHIM=1`. Use `MIKROTIK_HTTP=1` for plain HTTP.
- **Live cache (`lsp/src/server.rs`)**: `didChange` no longer wipes device snapshots (TTL 60s / negative 15s govern); invalidation only on `didClose`, connection-identity change (now including pin/CA), and `rsc.live.refresh`.
- **LSP startup banner (`lsp/src/main.rs`, `lsp/src/logging.rs`, `lsp/src/menus.rs`)**: Zed-only 4-line banner — start time (UTC) + pid + log level, dataset provenance (`RouterOS` version, menus, src hash), live status, encoding + effective TLS (WARN when insecure); every line carries a monotonic `[T+…s]` tag. No ANSI colors (Zed shows server stderr as plain text).

### Fixed

- **LSP live security (`lsp/src/live.rs`)**: `MIKROTIK_PASS` (and `pass`/`password`) in workspace settings is now ignored with a warning — env/keychain is the sole password source. Previously a secret in `.zed/settings.json` (a committable file) was silently ingested. **Behavior change**: live auth configured only via settings stops working; move the password to the environment/keychain.
- **Validator false positive (`lsp/src/diagnostics.rs`)**: space-separated `ubit` values (`rates=1Mbps, 2Mbps`) no longer flag the whitespace-split remainder; only genuinely empty members (`,`, `a,,b`) hint.
- **JSON parse errors (`lsp/src/server.rs`)**: malformed bodies answer `-32700 Parse error` (best-effort id echo) instead of hanging the client.
- **Live-check urllib fallback (`scripts/mikrotik-live-check.py`)**: TLS context now travels on `HTTPSHandler` — `OpenerDirector.open()` has no `context` kwarg, so the no-`requests` path always failed with `TypeError`.
- **Python test contracts (`tests/`)**: pins updated to the new behavior (live sort key `0!live_`, central `redact_secrets`, redirect-blocking mocks, fail-closed link-local); mocked `build_opener` transport for the urllib path.

## [0.5.5] - 2026-09-02

### Added

- **LSP hardening (`lsp/src/server.rs`, `lsp/src/diagnostics.rs`, `lsp/src/hover.rs`, `lsp/src/encoding.rs`, `lsp/src/parser.rs`, `lsp/src/caps.rs`, `lsp/src/live.rs`)**:
  - `C-01` percent-encoded `file://` URI decode with traversal re-validation (`%20`, `%2e%2e`).
  - `O-01` `truncated` Information diagnostic when `MAX_DIAG_BYTES 500KB` / `MAX_DIAG_LINES 3000` capped.
  - `P-01` `MAX_COMPLETION_ITEMS=200` bound.
  - `C-04` enum comma-list lenient `any` match (`chain=input,forward`).
  - `C-03` hover verb case-insensitive, `C-06` `**Read-only:**` section.
  - `P-02` `line_starts` memchr table for `lsp_position_to_offset`.
  - `A-01` unified `QuoteState` (`"`, `'`, `\`, `#`) for `scan_token`/`effective_content_end`/`walk_structure`.
  - `C-02` logical-line aware `completion.textEdit` via `LogicalLine::logical_offset_from_physical` + `map_range`.
  - `A-02` `Arc<MenuData>` zero-copy, `A-03` bounded fetches `MAX_CONCURRENT_FETCHES=2` (AtomicUsize), `A-05` caps registry, `P-04` `Arc<[String]>` live cache, `S-01` `RSC_LS_LIVE_ALLOW_LOOPBACK` SSRF flag, `O-02` hashed `uri_hash` + `latency` observability + startup banner `encoding/ssl_verify`.
- **Grammar hardening (`grammars/rsc/grammar.js`, `languages/rsc/*`)**:
  - `outline.scm` traverses `global_command_name`, `line_continuation` CRLF `\r?`, `command_substitution` multi-statement with GLR conflict, single-quote strings, `array_access prec3`, `function_call` non-recursive, `duration ms|us`, `ip_address` IPv6 tight, `config.toml ["#"] + "@"`, `(array)@indent`.
  - Highlights verb list `+monitor|watch|fetch|resolve|check|cancel|flush` (4 sites) + `GENERATED` header.
  - Corpus `72 → 79` (`menu_continuation`×2, `control_flow`×2, `variables $1/$:resolve`, `errors`×2 `("a" . \` + flat `$a $b $c`)).

### Changed

- **Data (`data/commands.toml`, `data/upstream-docs.toml`)**: `upstream 5503bd → c77198` — `26931e` TR069 CWMP enrichment (`+15 descriptions` for `/tr069-client`) + `c77198` file/fetch enrichment (`+182 lines` for `/file`, `/tool/fetch`), `1077 menus` stable `7.23.2`.
- **Grammar pin (`extension.toml`)**: `2fdfe88 → 24bcf71` (publishes `81998df` + `24bcf71`).
- **CI (`.github/workflows/ci.yml`, `release.yml`, `docs-drift.yml`, `security-audit.yml`)**: unified short names per OS — `Linux • check` / `Windows • check` / `macOS • check` (new) / `Docs • check` / `Grammar • check`; Release split into explicit per-target jobs (`Build Linux x86/arm64`, `Build macOS arm64/x86`, `Build Windows x86/arm64`, `Validations`, `Create Release`); watchdogs renamed `Docs Drift → Upstream Watchdog` / `Security Audit → Supply Audit` (`RustSec • audit`); per-asset `*.sha256`/`SHA256SUMS` generation restored (required by the shim's fail-closed download verification) with preflight gates, per-platform smoke runs and postflight companion self-verification.

### Fixed

- `test(sensor)` dead_code `cfg_with_no_loopback` clippy.
- **Live enrichment hardening**: WHATWG-normalized host validation (closes decimal/hex/compressed and IPv4-mapped IPv6 SSRF bypasses), deny `169.254.0.0/16` + `fe80::/10`, zero redirects, empty-fetch caching, explicit settings scope with host-change warning.
- **LSP**: `textDocument/rename` for script variables, bounded per-document parse cache, `MAX_DOCS` enforced on every `didOpen` branch, BOM stripped at open, live cache invalidated on change.
- **Data**: curated additive `data/overrides.toml` (seed: upstream-omitted `comment` on `/ip/route`).
- **Tests**: tautologies, silent skips and mocked subprocess checks replaced with real assertions; `test-python` fails without pytest.

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

[Unreleased]: https://github.com/balakar94/mikrotik-zed/compare/v0.5.6...HEAD
[0.5.6]: https://github.com/balakar94/mikrotik-zed/compare/v0.5.5...v0.5.6
[0.5.5]: https://github.com/balakar94/mikrotik-zed/compare/v0.5.3...v0.5.5
[0.5.3]: https://github.com/balakar94/mikrotik-zed/compare/v0.5.2...v0.5.3
[0.5.2]: https://github.com/balakar94/mikrotik-zed/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/balakar94/mikrotik-zed/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/balakar94/mikrotik-zed/compare/v0.4.0...v0.5.0
