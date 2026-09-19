# Caps & limits

Defensive bounds of `rsc-ls`. Single source of truth: `lsp/src/caps.rs`
(the header table there is documentary; the `const` wins). Configuration —
env vars, settings, live/deploy knobs — lives in
[Configuration](configuration.md).

Policy: bounded framing, bounded documents, bounded diagnostics, strict
`file://` URIs, no filesystem access beyond its cache.

## Protocol & document caps

| Cap | Value | Purpose |
| --- | --- | --- |
| `MAX_HEADER_SIZE` | 32 KiB | Content-Length header section cap per frame |
| `MAX_MESSAGE_SIZE` | 10 MiB | JSON-RPC body cap per frame |
| `MAX_DRAIN_SIZE` | 20 MiB | Declared body above this terminates the session (no unbounded drain) |
| `MAX_DOC_SIZE` | 5 MiB | tracked document size (truncate at char boundary) |
| `MAX_DOCS` | 100 | tracked open-document count |
| `MAX_CHANGES_PER_NOTIFICATION` | 512 | `contentChanges` entries accepted per `didChange` |

## Diagnostics & response caps

| Cap | Value | Purpose |
| --- | --- | --- |
| `MAX_DIAG_LINES` | 3000 | logical lines considered per doc |
| `MAX_DIAG_BYTES` | 500 000 | bytes considered per doc |
| `MAX_DIAGNOSTICS` | 2000 | semantic diagnostics per publish |
| `MAX_SYNTAX_DIAGNOSTICS` | 10 | brace/quote diagnostics per publish |
| `MAX_DIAG_TEXT_CHARS` | 120 | raw user text embedded in one diagnostic |
| `MAX_COMPLETION_ITEMS` | 200 | completion items per response |
| `MAX_CODE_ACTIONS` | 8 | quick-fix actions per response |
| `MAX_SYMBOLS` / `MAX_FOLDING_RANGES` | 5000 / 5000 | symbols / folds per doc |
| `MAX_REFERENCES` | 1000 | references per request |

## Hover, signature, completion display caps

| Cap | Value | Purpose |
| --- | --- | --- |
| `MAX_HOVER_PROPERTIES` | 12 | list entries **per hover section** (arguments, flags, read-only) |
| `MAX_HOVER_DESC_CHARS` | 800 | description chars embedded in a hover card (overlong text is marked `(truncated)`) |
| `MAX_DETAIL_CHARS` / `MAX_DETAIL_TYPE_CHARS` | 256 / 64 | single-line completion `detail` budget |
| `MAX_SIGNATURE_PROPERTIES` | 40 | properties per signature label |
| `MAX_SIGNATURE_LABEL_BYTES` | 4096 | total signature label budget |
| `MAX_SUGGEST_INPUT_BYTES` / `MAX_SUGGESTIONS_PER_PUBLISH` | 256 / 100 | quick-fix input and per-publish evaluation budget |

See [language features](language-features.md) for what these bound.

## Live-timing caps (also relevant offline)

| Cap | Value |
| --- | --- |
| `LIVE_TIMEOUT_SECS` | 5 s per-request default, clamped 1..30 s (`MIKROTIK_TIMEOUT`) |
| `LIVE_FETCH_BLOCKING_TIMEOUT_SECS` | 2 s max blocking fetch / coalescing window |
| `LIVE_NEGATIVE_TTL_SECS` | 15 s negative cache |

Full live caps (response/item/host/resource/TTL): [live-enrichment.md](live-enrichment.md#caps).

## Logging

Server logs go to **stderr** (stdout belongs to the protocol) with the
prefix `[rsc-ls][LEVEL][T+…s]`, where the tag is elapsed time since start:

```bash
RSC_LS_LOG=debug zed --foreground   # or RUST_LOG
# levels: error < warn < info < debug < trace (default info)
```

Zed surfaces the same stream via Command Palette → `zed: open log` and
`zed: open language server logs`. Never put secrets in settings — see
[Configuration](configuration.md#how-settings-reach-the-language-server).
