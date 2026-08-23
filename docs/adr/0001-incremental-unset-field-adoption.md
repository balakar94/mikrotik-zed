# ADR 0001: Incremental adoption of the dormant `unset` field

**Status:** Accepted (2026-08-23)

## Context

The extraction pipeline (`llms-full.txt` → `data/commands.toml`; provenance — RouterOS version, UTC timestamp, source SHA256 — is documented in the generated header of `data/commands.toml`) preserves an upstream-docs marker: properties whose documentation says the value can be removed again with `unset` carry `unset = true`. Currently ~1007 entries in the generated table have it.

The language server parses the field today (`RawArgEntry.unset` → `ArgEntry.unset` in `lsp/src/menus.rs`), but no rule consumes it yet, so the compiler flags it as dead code. The warning is silenced with `#[allow(dead_code)]` plus an explanatory comment on the field itself.

Deleting the field was considered and rejected: it is real data extracted from upstream docs. Dropping it would lose that signal, desync the parser from the generated schema, and force re-extraction churn once the first consumer inevitably lands.

## Decision

- **Keep** the field embedded in `data/commands.toml` and parsed into `ArgEntry`.
- **Adopt consumers incrementally, one per PR** (e.g., a hover hint, a diagnostics hint, or a completion affordance). Each consumer lands as its own PR and ships with tests.
- The `#[allow(dead_code)]` on `ArgEntry.unset` **stays until the last consumer lands**, then is removed together with that final consumer.

## Consequences

Positive:

- Roadmap for the field is visible and reviewable instead of implicit.
- Prevents accidental deletion of real upstream data; the extraction schema stays stable across regenerations.

Negative / accepted limitations:

- Temporary lint suppression (`#[allow(dead_code)]`) lives in `menus.rs` until full adoption.
- The unused field adds a small amount of embedded-binary size (~1007 × bool) until consumers justify keeping it.

Anchor points: `lsp/src/menus.rs` (`ArgEntry.unset` field with its `#[allow(dead_code)]` comment) and the generated header of `data/commands.toml` (extraction provenance).
