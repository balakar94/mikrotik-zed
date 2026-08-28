# Roadmap

Lightweight, direction-only plan for `mikrotik-zed` — Zed extension for MikroTik RouterOS 7.20+ script (tree-sitter + `rsc-ls`). No dates are promised; order reflects current intent, not commitment. For shipped changes see [CHANGELOG.md](CHANGELOG.md) and [docs/adr/](docs/adr/).

## Overview

Three independent tracks advance together: tree-sitter grammar (`grammars/rsc/` → [tree-sitter-rsc](https://github.com/balakar94/tree-sitter-rsc)), Zed language definition (`languages/rsc/` — highlights, brackets, indents, outline), and language server (`lsp/src/` → `rsc-ls`) glued by `src/lib.rs` (WASM shim).

The command database (`data/commands.toml`) is generated from upstream docs (`llms-full.txt` → `data/commands.toml`) and embedded at compile time via `include_str!()`. Remote branches prefixed `feat/` indicate explored directions; listing them does not imply merge order or guarantee.

Volatile facts (pinned grammar `rev`, MSRV, coverage counts) are not duplicated
— check `extension.toml`, `rust-toolchain.toml`, and `data/commands.toml` headers.
See [docs/publishing-runbook.md](docs/publishing-runbook.md) for release mechanics.
This roadmap is intentionally coarse; details live in issues and PRs.

## Now — 0.5.x Stabilization

Focus is correctness, hardening, and release hygiene before any feature expansion.

- **0.5.2 — shipped 2026-08-27** (tag `v0.5.2`): stability and docs sync — deduplicated wifi args, flag type emission, hover fallback, version docs clarified (snapshot 7.23.2), upstream sync.
- **0.5.1 prerelease — shipped 2026-08-26** (tag `v0.5.1`): published as a prerelease GitHub Release with all six platform binaries + SHA-256 companions + `extension.wasm`. Registry submission is deferred until the marketplace review window; see [docs/publishing-runbook.md](docs/publishing-runbook.md).
- **0.5.3 — Live Hardening (staged in `feat/live-data`, `98b24a9`)**: enriched connection system hardened for production — non-blocking hydrator (stale-while-revalidate, 2s coalescing), 15s negative-cache circuit breaker, real `MIKROTIK_SSL=0` rustls insecure verifier with `OnceLock` agent reuse, robust `url`-crate URL building + IPv6 bracketing + SSRF denial (`169.254.169.254`/`metadata`), multi-host `MIKROTIK_HOST="a,b,c"` (`LIVE_MAX_HOSTS=4`), generic dispatcher via `RSC_LS_LIVE_RESOURCES` (`LIVE_CUSTOM_RESOURCES_MAX=8`), `rsc.live.refresh`/`status` commands + `workspace/didChangeConfiguration` hot-reload, structured observability, and real health check `scripts/mikrotik-live-check.py` (mirrors `LiveConfig`, `languages/rsc/tasks.json` + `.zed/tasks.json` now 6 tasks, 2 live).
- **Shim cache integrity / download verification** (`feat/shim-download-verification`, Phase 1 in `src/cache.rs` / `src/verify.rs`): versioned layout `rsc-ls-<version>` (`.exe` on Windows), `.verified` marker, re-hash on reuse, clean abort on mismatch.
- **Grammar token and highlight fixes** in prerelease: `mac_address` / `duration`, `$1` positional, `boolean_literal` / `array_access` precedence, `highlights.scm` corrections (`feat/multiline-string-grammar`, `feat/highlight-field-colors`) — mirrored to `grammars/rsc/queries/` and covered by corpus `68/68`.
- **LSP framing / diagnostics hardening:** bounded `MAX_HEADER_SIZE`, `SyntaxFinding` deferred materialization for backslash continuations, `didChange` batch handling, duplicate `id` detection.
- **CI / extraction hygiene:** `make validate` gates `commands.toml` idempotency, `make sync-check` guards upstream drift, release builds use `cargo --locked`.

No new user-visible LSP features are targeted in 0.5.x beyond what is already staged.
Patch releases during this phase are prerelease-gated and registry-deferred until binaries are live.

## Next — 0.6.0

Candidate set drawn from active `feat/` branches; each ships behind tests and `make validate`. Scope may narrow or defer — no commitment.

- **LSP additive features** (`feat/lsp-additive-features`): extended completion and hover coverage for menus still at shallow depth.
- **Diagnostics and assists:** syntax diagnostics (`feat/lsp-syntax-diagnostics`), enum quick-fixes (`feat/lsp-enum-quickfixes`), did-you-mean corrections (`feat/lsp-did-you-mean`) — edit-distance suggestions for properties/menus/values.
- **Language intelligence:** signature help (`feat/lsp-signature-help`) with required-first hints, variable navigation (`feat/lsp-variable-navigation`) and variable highlighting (`feat/variable-highlighting`), plus colon trigger (`feat/colon-trigger-and-readme`).
- **Quality / observability:** end-to-end harness (`feat/lsp-e2e-harness`), binary observability/logging (`feat/lsp-binary-observability`).
- **Grammar:** multiline string support (`feat/multiline-string-grammar`) if corpus-stable; otherwise stays exploratory.
- **Incremental `unset` adoption:** per [ADR 0001](docs/adr/0001-incremental-unset-field-adoption.md), consumers of `ArgEntry.unset` land one per PR (hover / diagnostic / completion) until `#[allow(dead_code)]` can be removed. See also [docs/adr/](docs/adr/).

Windows support landed in 0.5.0/0.5.1 (auto-download, `.exe` handling, `windows-arm64` + `x86_64-pc-windows-msvc` via `feat/windows-auto-download`, `feat/windows-arm64`, `feat/windows-support`); no further Windows work planned for 0.6.0 unless regressions appear.

## Vision — Live Data (early, opt-in) — `feat/live-data` — hardened in 0.5.3

> **Opt-in live device enrichment via Generic Resource Dispatcher.** Shipped as opt-in in `feat/live-data` (0.5.3, `98b24a9`); hardening keeps it zero-disk and bounded. Explores live enrichment directly from RouterOS devices over REST without touching `data/commands.toml`.

**Shipped (0.5.3, `feat/live-data`):** `rsc-ls` connects to a configured RouterOS device over REST and dynamically enriches autocomplete with **real-time values from the device**:
- **Interfaces & Bridges:** `interface`, `bridge`, `in-interface`, `out-interface`, `parent`
- **IP Addresses & Networks:** `address`, `network`, `src-address`, `dst-address`, `gateway`, `to-addresses`
- **Firewall & Lists:** `src-address-list`, `dst-address-list`, `address-list`, `list`, `chain`, `jump-target` across Filter, NAT, Mangle, and Raw
- **IP Pools:** `pool`, `address-pool`, `remote-pool` (IPv4 and IPv6)
- **Hardening:** non-blocking hydrator (stale-while-revalidate + 2s coalescing), 15s negative-cache breaker, `url`-crate + IPv6 bracketing + SSRF denial, `MIKROTIK_SSL=0` real verifier bypass, multi-host (`LIVE_MAX_HOSTS=4`), custom `RSC_LS_LIVE_RESOURCES` (`LIVE_CUSTOM_RESOURCES_MAX=8`), `rsc.live.refresh`/`status` + hot-reload, `scripts/mikrotik-live-check.py` health check.

**Future Hydration Vectors (Post-0.6.0 candidate vectors):**
* **Routing & BGP:** Dynamic BGP peers, AS numbers, VRF instances, and routing tables (`routing-table`, `vrf`, `instance`, `peer`).
* **Wireless & CAPsMAN:** Wi-Fi channel profiles, security profiles, datapaths, and steering rules (`configuration`, `security`, `channel`, `datapath`).
* **Queues & QoS:** Simple queue targets, queue tree parents, packet marks, and connection marks (`packet-mark`, `connection-mark`, `routing-mark`).
* **Certificates & Security:** Installed TLS certificates, trust chains, and user groups (`certificate`, `ca-certificate`, `group`).
* **DHCP & Network Services:** DHCP servers, option sets, and static DNS hostnames (`dhcp-server`, `dhcp-options`).

**Architectural & Security Guarantees (now enforced):**
* **Zero disk modifications:** Live values live strictly in an in-memory `LiveCache` (TTL 60s, max 16 collections, max 500 items, max 64 chars per value, 512 KiB cap). The static snapshot (`data/commands.toml` 7.23.2) remains the single source of truth for command grammar and structure.
* **Bounded execution budget:** Completion never blocks the LSP loop; background hydrator is coalesced (2s) and negative-cached (15s) so the editor never stutters or hammers the router when offline.
* **Credential safety:** Credentials (`MIKROTIK_PASS`) are read from process environment / keychain only and are strictly redacted from all debug logs and errors; `url` building is bracket-safe and SSRF-aware.
* **Zed configuration:** Opt-in via `RSC_LS_LIVE=1` / `MIKROTIK_LIVE=1` with host/user/pass configured via Zed `settings.json`, shell environment, or interactive Zed tasks in `languages/rsc/tasks.json` (now 6 tasks, 2 live) + `rsc.live.status`/`refresh` commands and hot-reload via `workspace/didChangeConfiguration`.

## Later — 1.0+

Exploratory, not committed. Considered only after 0.6.0 stabilizes:

- Full Windows auto-download hardening and ARM64 CI coverage consolidation.
- Grammar and query convergence with upstream Zed highlight/indent changes.
- Extraction pipeline polish (entity decoding, atomic writes, drift detection already in 0.5.1) extended to new RouterOS documentation shapes.
- Remaining `feat/` branches not absorbed into 0.6.0, re-evaluated against user feedback and real `/export` validation on devices.

A 1.0 would require stable grammar pinning, completed `unset` adoption, and
sustained green on `make validate` + `make sync-check` — no calendar target.
Until then, `feat/` branches remain the lab for ideas, not a promise.

## Principles

- **No date promises.** Milestones are ordering hints; [CHANGELOG.md](CHANGELOG.md) is source of truth for what shipped.
- **Hard rules stay hard:** no bundled `rsc-ls` binary, `wasm32-wasip2`-clean shim, schema-known `extension.toml` keys only.
- **Edit inputs, not outputs:** change `grammar.js` / upstream docs, then regenerate `parser.c` / `data/commands.toml`; never hand-edit generated files.
- **Defensive LSP:** capped messages/docs/diagnostics, bounded document store, strict `file://` validation. See [docs/adr/](docs/adr/) — especially [ADR 0001](docs/adr/0001-incremental-unset-field-adoption.md) for incremental adoption.
- **Separate pipelines:** grammar semantics and command data evolve independently; coupled changes are avoided.
- **Verify before publishing:** `make validate`, install dev extension in Zed, check `zed: open log`; registry PRs follow [docs/publishing-runbook.md](docs/publishing-runbook.md).
