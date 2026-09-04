// ── Resource limits — single source of truth ─────────────────────
//
// Every shared resource cap of the language server is declared here
// exactly once and re-exported through the crate root (`main.rs`), so
// callers keep their historical paths (`crate::MAX_DOC_SIZE`, …) while
// the values themselves can no longer drift between modules.
//
// Policy intent (repo hard rule #7 — "the LSP stays defensive"):
// - Message framing is capped (header section + body) so a hostile
//   client cannot drive unbounded allocation or flood us with
//   never-terminating headers.
// - Tracked documents are bounded in size and count; oversized payloads
//   are truncated at char boundaries instead of being stored wholesale.
// - Diagnostic work is bounded per document (lines + bytes).
// - Quick-fix responses stay bounded even when a client echoes hundreds
//   of eligible diagnostics.
//
// Central caps registry — shared caps live here; feature-internal
// micro-caps live beside their feature but are indexed here so every
// limit is discoverable from ONE place. Value column is authoritative
// via the defining `const`; this table is documentary (re-export allies
// may lag, but the const wins).
//
// | Cap                               | Value          | Defined in      | Purpose                                                        |
// |-----------------------------------|----------------|-----------------|----------------------------------------------------------------|
// | `MAX_HEADER_SIZE`                 | 32 KiB         | caps.rs         | Content-Length header section cap per frame                      |
// | `MAX_MESSAGE_SIZE`                | 10 MiB         | caps.rs         | JSON-RPC body cap per frame                                    |
// | `MAX_DOC_SIZE`                    | 5 MiB          | caps.rs         | Tracked document size (truncate at char boundary)              |
// | `MAX_DOCS`                        | 100            | caps.rs         | Tracked open-document count                                    |
// | `MAX_CODE_ACTIONS`                | 8              | caps.rs         | Quick-fix actions per codeAction response                      |
// | `MAX_DIAG_LINES`                  | 3000           | caps.rs         | Logical lines considered for diagnostics per doc               |
// | `MAX_DIAG_BYTES`                  | 500 000        | caps.rs         | Bytes considered for diagnostics per doc                       |
// | `MAX_DIAGNOSTICS`                 | 2000           | caps.rs         | Total semantic diagnostics per publish                         |
// | `MAX_COMPLETION_ITEMS`            | 200            | caps.rs         | Completion items per response                                  |
// | `MAX_LIVE_ITEMS`                  | 500            | caps.rs         | Live values per cache entry                                    |
// | `MAX_LIVE_VALUE_LEN`              | 64             | caps.rs         | Single live value byte length                                  |
// | `MAX_LIVE_RESPONSE_BYTES`         | 512 KiB        | caps.rs         | Live HTTP response cap                                         |
// | `MAX_CACHE_ENTRIES`               | 16             | caps.rs         | Distinct live cache keys                                       |
// | `LIVE_TTL_SECS`                   | 60 s           | caps.rs         | Live entry TTL                                                 |
// | `LIVE_TIMEOUT_SECS`               | 5 s            | caps.rs         | Live per-request timeout (clamped 1..30)                       |
// | `LIVE_FETCH_BLOCKING_TIMEOUT_SECS`| 2 s            | caps.rs         | Coalescing window & max blocking fetch                         |
// | `LIVE_NEGATIVE_TTL_SECS`          | 15 s           | caps.rs         | Negative cache TTL                                             |
// | `LIVE_MAX_HOSTS`                  | 4              | caps.rs         | Multi-host cap (only primary hydrated)                         |
// | `LIVE_CUSTOM_RESOURCES_MAX`       | 8              | caps.rs         | Custom live resources via env JSON                             |
// | `MAX_CONCURRENT_FETCHES`          | 2              | live.rs         | fetch-thread semaphore                                         |
// | `MAX_SYNTAX_DIAGNOSTICS`          | 10             | diagnostics.rs  | Unclosed/unmatched brace+quote diagnostics per publish         |
// | `MAX_SYMBOLS`                     | 5000           | symbols.rs      | Document symbols per doc                                       |
// | `MAX_FOLDING_RANGES`              | 5000           | folding.rs      | Folding ranges per doc                                         |
// | `MAX_REFERENCES`                  | 1000           | navigation.rs   | References per request (incl. declaration)                     |
// | `MAX_BRACE_DEPTH`                 | 4096           | parser.rs       | Open-brace stack bound (walk_structure + syntax)               |
// | `MAX_SIGNATURE_PROPERTIES`        | 40             | signature.rs    | Properties per signature label                                 |
// | `MAX_SUGGEST_INPUT_BYTES`         | 256            | suggest.rs      | Max mistyped token length for quick-fix suggestions            |
//
// Scattered micro-caps deliberately stay beside their features —
// they tune one algorithm rather than a cross-module resource budget,
// but they are listed above so the caps story is centralized.

/// Hard cap on the header section of one frame.
pub(crate) const MAX_HEADER_SIZE: usize = 32 * 1024; // 32 KiB
/// Cap on one JSON-RPC message body; larger bodies are drained and skipped.
pub(crate) const MAX_MESSAGE_SIZE: usize = 10 * 1024 * 1024;

pub(crate) const MAX_DOC_SIZE: usize = 5 * 1024 * 1024; // 5 MiB per document — prevents single-file OOM
pub(crate) const MAX_DOCS: usize = 100; // cap number of tracked documents
/// Cap on quick-fix actions returned per `textDocument/codeAction` request.
///
/// Bounds the response even when a client echoes hundreds of eligible
/// diagnostics (e.g. after pasting a large broken script); eight one-click
/// fixes is already beyond what an editor surfaces comfortably.
pub(crate) const MAX_CODE_ACTIONS: usize = 8;

pub(crate) const MAX_DIAG_LINES: usize = 3000;
pub(crate) const MAX_DIAG_BYTES: usize = 500_000; // cap per-doc bytes considered for diagnostics

/// Cap on total semantic diagnostics per publish.
///
/// Bounds the otherwise uncapped per-property loop in `compute_diagnostics`:
/// a single logical line carrying tens of thousands of distinct unknown keys
/// would allocate one heap `Diagnostic` per key (50k+ findings from one
/// line). 2000 sits well above any legitimate file's distinct findings (real
/// scripts yield tens; even a 3000-line file capped by `MAX_DIAG_LINES`
/// rarely approaches it) yet far below that pathological case, and keeps the
/// publish payload bounded.
pub(crate) const MAX_DIAGNOSTICS: usize = 2000;

/// Maximum number of completion items returned per `textDocument/completion` request.
///
/// Bounds the response payload and keeps client fuzzy filtering responsive;
/// 200 covers even large menu/path sets with headroom while staying well
/// below the typical client limits for a single completion response.
pub(crate) const MAX_COMPLETION_ITEMS: usize = 200;

// ── Live device data caps ─────────────────────────────────────────
// These bound the in-memory, TTL-scoped cache for RouterOS live data
// (never persisted, never overwrites `data/commands.toml`). They keep
// completion enrichment safe against hostile or oversized device responses.

/// Maximum number of live interface names retained per cache entry.
///
/// Truncation keeps completion payloads bounded and the client fuzzy
/// filter responsive; 500 covers even large CCR deployments with headroom.
pub(crate) const MAX_LIVE_ITEMS: usize = 500;

/// Maximum byte length of a single live value (interface name).
///
/// Matches RouterOS interface-name limits and prevents a single rogue
/// entry from dominating the cache.
pub(crate) const MAX_LIVE_VALUE_LEN: usize = 64;

/// Maximum bytes accepted from a live device response before it is
/// discarded.
///
/// Protects against OOM on a malicious or misconfigured endpoint
/// returning an unbounded JSON array.
pub(crate) const MAX_LIVE_RESPONSE_BYTES: usize = 512 * 1024; // 512 KiB

/// Maximum number of distinct cache keys retained in memory.
///
/// Each key maps to one live collection (e.g. `"interfaces"`, `"ip_addresses"`,
/// `"address_lists"`, `"firewall_chains"`, `"ip_pools"`); sixteen entries
/// keeps the cache small while supporting all live resources concurrently.
pub(crate) const MAX_CACHE_ENTRIES: usize = 16;

/// Time-to-live for a cached live entry before it is considered stale.
///
/// Sixty seconds balances freshness for rapidly changing interface sets
/// against request amplification on flapping completions.
pub(crate) const LIVE_TTL_SECS: u64 = 60;

/// Default per-request timeout for live device fetches (seconds).
///
/// Shorter than the deploy companion's 60 s (see `scripts/mikrotik-deploy.py`)
/// because completion is latency-sensitive; clamped to 1..30 s.
pub(crate) const LIVE_TIMEOUT_SECS: u64 = 5;

/// Maximum blocking time the completion handler may spend waiting for
/// a live fetch before falling back to the honest (no live data) result.
pub(crate) const LIVE_FETCH_BLOCKING_TIMEOUT_SECS: u64 = 2;

/// Time-to-live for negative cache entries (failed fetches) before retry is allowed.
///
/// Prevents immediate retry spam after a device failure; completion stays
/// non-blocking and the next fetch is deferred until this window expires.
pub(crate) const LIVE_NEGATIVE_TTL_SECS: u64 = 15;

/// Maximum number of hosts in multi-host mode (comma-separated `MIKROTIK_HOST`).
///
/// Validated but only primary host hydrated; multi-host is future
/// iteration. Additional hosts up to `LIVE_MAX_HOSTS` are parsed and
/// validated (SSRF/host rules) but no fetch is issued for them today;
/// single-host mode is the only hydrated path.
pub(crate) const LIVE_MAX_HOSTS: usize = 4;

/// Maximum number of user-defined custom live resources via `RSC_LS_LIVE_RESOURCES`.
///
/// Bounds parsing of the JSON env var to avoid unbounded allocation.
pub(crate) const LIVE_CUSTOM_RESOURCES_MAX: usize = 8;
