# LSP config & caps

Single source of truth: `lsp/src/caps.rs` (header table is documentary;
the `const` wins). Policy: the LSP stays defensive — bounded framing,
bounded documents, bounded diagnostics, strict `file://` URIs, no
filesystem access beyond its cache.

## Protocol & document caps

| Cap | Value | Purpose |
| --- | --- | --- |
| `MAX_HEADER_SIZE` | 32 KiB | Content-Length header section cap per frame |
| `MAX_MESSAGE_SIZE` | 10 MiB | JSON-RPC body cap per frame (larger drained + skipped) |
| `MAX_DOC_SIZE` | 5 MiB | tracked document size (truncate at char boundary) |
| `MAX_DOCS` | 100 | tracked open-document count |

## Diagnostics & response caps

| Cap | Value | Purpose |
| --- | --- | --- |
| `MAX_DIAG_LINES` | 3000 | logical lines considered per doc |
| `MAX_DIAG_BYTES` | 500 000 | bytes considered per doc |
| `MAX_DIAGNOSTICS` | 2000 | semantic diagnostics per publish |
| `MAX_SYNTAX_DIAGNOSTICS` | 10 | brace/quote diagnostics per publish |
| `MAX_COMPLETION_ITEMS` | 200 | completion items per response |
| `MAX_CODE_ACTIONS` | 8 | quick-fix actions per response |
| `MAX_SYMBOLS` / `MAX_FOLDING_RANGES` | 5000 / 5000 | symbols / folds per doc |
| `MAX_REFERENCES` | 1000 | references per request |

See [language features](language-features.md) for what these bound.

## Live-timing caps (also relevant offline)

| Cap | Value |
| --- | --- |
| `LIVE_TIMEOUT_SECS` | 5 s per-request default, clamped 1..30 s (`MIKROTIK_TIMEOUT`) |
| `LIVE_FETCH_BLOCKING_TIMEOUT_SECS` | 2 s max blocking fetch / coalescing window |
| `LIVE_NEGATIVE_TTL_SECS` | 15 s negative cache |

Full live caps (response/item/host/resource/TTL): [live-enrichment.md](live-enrichment.md#caps).

## Logging

Server logs to **stderr** (stdout belongs to the protocol):

```bash
RSC_LS_LOG=debug zed --foreground   # or RUST_LOG
# levels: error < warn < info < debug < trace (default info)
# prefixes: [rsc-ls][LEVEL] and [mikrotik-zed]
```

Source: `lsp/src/logging.rs`, `lsp/src/cli.rs`. Never put secrets in
settings — see [live trust gate](live-enrichment.md#settings-trust-gate).
Zed surface: Command Palette → `zed: open log` / `zed: open language server logs`.
