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
// Feature-internal micro-caps deliberately stay beside their features —
// they tune one algorithm rather than a cross-module resource budget:
// `MAX_SYNTAX_DIAGNOSTICS` (diagnostics.rs), `MAX_SYMBOLS` (symbols.rs),
// `MAX_FOLDING_RANGES` (folding.rs), `MAX_REFERENCES` (navigation.rs),
// `MAX_BRACE_DEPTH` (parser.rs), `MAX_SIGNATURE_PROPERTIES`
// (signature.rs), `MAX_SUGGEST_INPUT_BYTES` (suggest.rs).

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
