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

## Vision — Live Data (early, opt-in) — `feat/live-data`

> **Early vision, not committed.** Branch `feat/live-data` explores live enrichment without touching `data/commands.toml`.

**Goal:** when the user opts in, Zed asks for MikroTik credentials once (via task input / settings) and `rsc-ls` enriches completion/hover with **live values from the device** (interfaces, addresses, bridges) via REST, cached in memory with TTL. The snapshot (`data/commands.toml` 7.23.2) stays the single source of truth for structure; live data never overwrites the file.

**How it would surface in Zed (no file modification):**
* **No `data/commands.toml` edit** — live values live only in `rsc-ls` memory (or `data/live-cache.toml` gitignored if persisted), never committed. `make extract` stays idempotent.
* **Contextual menu / task is the Zed-native way:** `languages/rsc/tasks.json` already exposes `task: spawn` with `inputs` prompts (`host`, `user`, `pass` → `MIKROTIK_HOST` env). WASM shim cannot show a native `prompt()` dialog, but a `context_menu` entry / slash command that triggers the `MikroTik: Connect Live` task gives the same UX: right-click `.rsc` → Connect Live → Zed shows input boxes, sets env for the LSP, live cache warms. No file is dirtied.

**Scope if validated:** `interface=` / `bridge=` / `address=` live completions, hover with `actual-interface` from device, diagnostics `unknown interface (not on this device)` — all behind `live=false` default, fallback to snapshot on fetch failure. See `docs/adr/` for future ADR.

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
