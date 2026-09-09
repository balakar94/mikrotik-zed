# QA · CI · Release

`make help` is the canonical target list. CI mirrors these targets 1:1.

## Gates

| Target | What it runs | When |
| --- | --- | --- |
| `make check` | fast compile gate (WASM + LSP) | during development |
| `make validate-fast` | manifest + fmt + clippy + Rust tests | quick gate |
| `make validate` | offline gate: `check-manifest`, `docs-check`, `generate-check`, fmt, clippy, **all** tests, extract + idempotency assert | before claiming done / CI |
| `make sync-check` | upstream-docs staleness (network) | separate CI gate, not in `validate` |

Minimum verification by change type: see `AGENTS.md` table
(`lsp/src/**` → `make fmt clippy test-rust`; shim → `make check-wasm`
+ Install Dev Extension; `grammar.js` → generate + test; `highlights.scm`
→ mirror + smoke test; pipeline → `make extract` + diff; `docs/**` →
`make docs-check`).

## Test suites

- **Grammar:** `make test-grammar` (`npx tree-sitter test`, corpus in `grammars/rsc/`).
- **Rust:** `make test-rust` (`cargo test --workspace`) — completion tiers,
  live, framing codec, diagnostics, hover, navigation, encoding.
  Perf bounds run in `.github/workflows/perf.yml` (schedule-only: weekly
  Tue 05:17 UTC; manual `workflow_dispatch` opt-in for PRs with suspected
  perf regressions — Actions → Perf → Run workflow).
- **Python:** `make test-python` (`pytest tests/`) — environment, extraction,
  functionality, enclosure, release/manifest, tasks mirror, live opt-in.

## Release

Two independent tracks:

1. **GitHub Release (automated):** `make bump VERSION=x.y.z` syncs
   `Cargo.toml` / `lsp/Cargo.toml` / `extension.toml` (grammar crate versions
   are independent — never bumped from here), then
   `git tag vX.Y.Z && git push origin vX.Y.Z` fires `release.yml`
   (multi-platform `rsc-ls` + WASM → GitHub Release with SHA-256 companions
   that auto-download verifies).
2. **Zed Marketplace (human-reviewed):** PR to `zed-industries/extensions`
   (one extension per PR, ≤3 open, reply ≤3 weeks).
   Checklist: [publishing-runbook.md](publishing-runbook.md);
   local gate: `make check-manifest`.

Grammar publish rides its own script (`publish_grammar.py --push`) and
updates the `extension.toml` pin itself — see [grammar.md](grammar.md).
