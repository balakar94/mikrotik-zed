# Skill: RSC Language Server

## When to Use

Trigger this skill when the task involves any of: `completion`, `hover`, `diagnostics`, `publishDiagnostics`, `textDocument/*`, `LSP`, `rsc-ls`, `menus`, `commands.toml`, `MenuData`, `LineContext`, `WASM extension`, `language_server_command`, auto-download, or editing files under `lsp/src/` / `src/lib.rs`.

## Purpose

Guide for the pure-Rust language server `rsc-ls` and WASM extension that provide completion, hover, and diagnostics for MikroTik RouterOS Script (`.rsc`) in Zed. Complete CLI coverage embedded via `include_str!()` — current menu count in the `data/commands.toml` header.

## State

Working end-to-end: workspace member `lsp/` (native binary `rsc-ls`) + WASM `cdylib` `src/lib.rs` (`zed_extension_api 0.7`). Diagnostics push+pull, 5 rules; completion with statement snippets; flat document symbols; folding ranges. Auto-download from GitHub Releases with PATH fallback and 4 platform triples.

## Architecture

### Data Flow

```
.rsc file → Zed → WASM Extension (src/lib.rs) → stdio JSON-RPC 2.0 → rsc-ls (lsp/src/main.rs)
  language_server_command(): worktree.which("rsc-ls") → cached ext dir → current_platform() → download
  rsc-ls: BufReader stdin → parse Content-Length → JSON-RPC dispatch → docs: HashMap<uri, String>
       → parse_line/build_before_cursor → MenuData lookup → completion/hover/diagnostics
       → stdout: Content-Length framing ← Zed renders
  data/commands.toml embedded at compile time → MenuData::load() → menu_by_path + child_names_by_parent
```

Protocol: `Content-Length` framing, `textDocumentSync = {"openClose": true, "change": 2}` (Incremental), `triggerCharacters ["/"," ","="]`, `diagnosticProvider {interFileDependencies:false}`. Range-scoped edits are the authoritative path (`apply_incremental_edit`); full-text replacements and failed patches fall back to a full document replace. The server negotiates `positionEncoding` during `initialize` (prefers utf-8, falls back to utf-16 per LSP 3.17 default) and keeps all internal position math byte-based, converting only at the protocol boundary (`lsp_character_to_byte_offset`, `convert_diagnostic_ranges`).

### File Responsibilities

| File | Role |
|------|------|
| `src/lib.rs` | WASM extension; `RscExtension::language_server_command` (PATH→cache→download), `platform_triple`, `LanguageServerInstallationStatus` |
| `lsp/src/main.rs` | LSP stdio loop + `Server::handle_message` (all method dispatch), doc store caps, `publish_diagnostics`; re-exports the extracted modules |
| `lsp/src/cli.rs` | CLI flags (`--version`/`-V`, `--help`/`-h`, usage errors exit 2), `version_string()` (`RSC_LS_BUILD_SHA` → ` (build <sha7>)`); handled before any logging/loading |
| `lsp/src/framing.rs` | `Frame`/`FrameError`/`read_message` — Content-Length framing with header/body caps; unparsable headers are terminal (`FrameError::Protocol`) to prevent desync cascades |
| `lsp/src/parser.rs` | Quote-aware `scan_token`/`SpanToken`/`tokenize_with_spans`/`tokenize`, `parse_line`, multi-line `build_before_cursor`; whole-document `walk_structure` + `StructureEvent` (quote/comment state machine shared by folding and the syntax diagnostics) |
| `lsp/src/encoding.rs` | `PositionEncoding` negotiation, byte↔UTF-16 conversions, `apply_incremental_edit`, diagnostic/symbol position conversion at the protocol boundary |
| `lsp/src/logging.rs` | `log_error!…log_trace!` macros + `RSC_LS_LOG` level gate (macros resolve `crate::logging::` so any module can use them) |
| `lsp/src/menus.rs` | `MenuData::load()` / `from_toml_str` (test-only), `COMMANDS_TOML`, validation (≤256, charset, `..`, control), `menu_by_path`, `child_names_by_parent`, `STANDARD_VERBS` (15) |
| `lsp/src/completion.rs` | `compute_completions` → root/sub-menu/verb/arg/value + statement-start snippet templates; `kind` CLASS/FUNCTION/PROPERTY/CONSTANT/ENUM_MEMBER/SNIPPET |
| `lsp/src/hover.rs` | `compute_hover` (menu→property→flag→verb), `find_word_start/end` (includes `/ - _`, UTF-8 safe) |
| `lsp/src/diagnostics.rs` | `compute_diagnostics` — 7 rules (5 menu + 2 syntax: unclosed brace/quote family), `source="rsc-ls"`, capped; also owns `logical_lines()` (continuation joining) shared with symbols/folding, and `resolve_menu_for_line()` (menu context for a physical line) used by code actions |
| `lsp/src/suggest.rs` | `damerau_levenshtein` (OSA variant), `suggestion_threshold`, `best_candidate` — deterministic did-you-mean engine behind code actions |
| `lsp/src/symbols.rs` | `compute_document_symbols` — flat symbol list: menu commands → Object(19), `:local`/`:global` → Variable(13) named by identifier, other `:verb` → Function(12); skips bare fragments/comments |
| `lsp/src/folding.rs` | `compute_folding_ranges` — quote/comment-aware brace regions (kind `"region"`), kindless folds for `\` continuations; only when `startLine < endLine` |
| `lsp/src/server.rs` | Helper module: URI validation (`is_valid_file_uri`), mirrored caps + enclosure/capacity invariant tests |
| `data/commands.toml` | Generated command table (header: version/timestamp/SHA256), truth source `llms-full.txt` |
| `extension.toml` | Manifest: `grammars[rsc]` + `language_servers[rsc-ls]` |

## Capabilities

Advertised in `initialize`: `positionEncoding`, `textDocumentSync {openClose:true, change:2}`, `completionProvider {triggerCharacters:["/", " ", "="]}`, `hoverProvider`, `documentSymbolProvider`, `foldingRangeProvider`, `codeActionProvider`, `diagnosticProvider {interFileDependencies:false}`.

### Completion (`textDocument/completion`)

Strategy: return all candidates, let Zed fuzzy-filter. Only exception: `property=` → value completions.

- No path → roots (`get_root_completion_items`, `kind::CLASS`)
- Before verb → sub-menus (`get_sub_menu_completion_items`) + `STANDARD_VERBS` + `Command` children (`get_verb_completion_items`, `kind::FUNCTION`)
- After verb (`add`/`print`) → args/flags (`get_arg_completion_items`, `kind::PROPERTY`/`CONSTANT`, snippets `address=$1` / `comment="$1"`, skips used props)
- After `key=` → enum/bool/`iface_enum`/`ipAddr`/`ipPrefix` values (`get_value_completions`, `kind::ENUM_MEMBER`)

Context: `build_before_cursor` + `parse_line` (both in `parser.rs`; `parse_line` uses `child_names_by_parent` for implicit parents like `/ip/firewall`).

Statement snippets (Stage B): at a statement start — no previous token on the logical line, or the previous quote-aware token is exactly `{` or `;` — with an empty resolved menu path and no trailing `/`, four template items (`:if`, `:foreach`, `:for`, `:do`; `kind::SNIPPET`, `insertTextFormat 2`, `sortText "9…"`) are appended to whatever base candidates apply. Strict token equality keeps them out of mid-command positions (`do={` is one token) and out of quoted strings. Note `:` is NOT a trigger character, so they surface on space-triggered requests at statement start.

### Document Symbols (`textDocument/documentSymbol`, `compute_document_symbols` in `symbols.rs`)

Flat `DocumentSymbol[]` (no children). Per logical line (`diagnostics::logical_lines`, so `\` continuations join first): leading `/…` token → menu command, `Object(19)` named by the path+verb substring exactly as written, `selectionRange` on the first path token; `:local`/`:global` → `Variable(13)` named by the identifier token; other `:verb` → `Function(12)` named by the verb. Bare values, lone properties, comments, and bare `/` are skipped. Ranges computed in byte coordinates via segment mapping, converted to the negotiated encoding at the handler (`convert_position`). Untracked URI → null result; malformed params → `-32602`.

### Folding Ranges (`textDocument/foldingRange`, `compute_folding_ranges` in `folding.rs`)

Two sources merged and sorted by `startLine`, emitted only when `startLine < endLine`: brace regions (quote/comment-aware scan with state carried across physical lines → `kind:"region"`; unterminated braces emit nothing) and `\` continuations spanning multiple physical lines (no kind). Line-only ranges need no encoding conversion.

### Hover (`textDocument/hover`, `compute_hover` in `hover.rs`)

Order: 1) menu path (`### /ip/address` + `Type:` + `Arguments:` + `Flags:`) 2) property (`**address** Type: ipPrefix`) 3) flag (`**X** — disabled`) 4) standard verb (`**add** — Standard RouterOS command`). Returns `Hover{contents:{kind:"markdown",value}}` or `None`.

### Diagnostics (push `publishDiagnostics` + pull `textDocument/diagnostic`, `compute_diagnostics` in `diagnostics.rs`)

`publish_diagnostics` called after `didOpen`/`didChange` (full+incremental), `didClose` clears. `diagnosticProvider` advertised in `initialize`. Skips `#` comments, `:` globals, `}`/`{`/`..`. Unknown menu suppresses further property checks on that line (no cascade).

### Code Actions (`textDocument/codeAction`, `compute_code_actions` in `main.rs` + `suggest.rs`)

Returns `quickfix` actions ("Did you mean 'X'?") for client-echoed diagnostics with `source="rsc-ls"` and code `unknown-property`/`unknown-menu`; the edit replaces the diagnostic's own range with the candidate. The typo'd token is recovered from the tracked document at the diagnostic range (never by parsing message text), and its governing menu is resolved with the same continuation-aware machinery as the diagnostic pipeline (`diagnostics::logical_lines` + `parse_line` + `menu_by_path`, factored as `diagnostics::resolve_menu_for_line`) — unknown-property candidates are that menu's `arguments ∪ flags ∪ read_only` names, unknown-menu candidates are all known paths, an unresolvable menu yields no action rather than a cross-menu guess. Suggestion policy: Damerau-Levenshtein (optimal string alignment) distance must be ≤ 1 for inputs of ≤ 4 characters and ≤ 2 otherwise (short identifiers must not pick up noisy matches), distance-0 "identity fixes" for stale diagnostics are rejected, and ties break to the lexicographically smallest candidate so results never depend on HashMap iteration order. Capped at 8 actions per request; untracked URI → empty array; malformed params → `-32602`.

## Diagnostics Rules

| # | Code | Severity | When | Example |
|---|------|----------|------|---------|
| 1 | `unknown-menu` | Warning (2) | `path` not in `menu_by_path` nor `child_names_by_parent`, not prefix of any menu | `/foo/bar add` → `Unknown menu '/foo/bar'` |
| 2 | `unknown-property` | Warning (2) | key not in `arguments ∪ flags ∪ read_only` for known menu | `/ip/address add bad=1` → `Unknown property 'bad' for '/ip/address'` |
| 3 | `missing-required` | Info (3) | `Directory`/`Settings Directory` + verb `add`/`set` missing `arg.required` | `/ip/address add` → `Missing required property 'address' for '/ip/address add'` |
| 4 | `duplicate-property` | Warning (2) | same key twice (range = second occurrence) | `address=1.1.1.1 address=2.2.2.2` → `Duplicate property 'address'` |
| 5 | `invalid-enum-value` | Hint (4) | `arg_type` starts with `enum` and value not in `enum (a \| b \| ...)` | `chain=invalid` → `Invalid value 'invalid' for 'chain' (expected one of: input \| forward \| output)` |
| 6 | `unclosed-brace` | Error (1) | `{` never closed before EOF; range = that brace char; companion code `unmatched-brace` flags a stray `}` with no open `{`. Shares the quote/comment-aware walk with folding (`parser::walk_structure`) — braces inside strings/comments are inert, `\` continuations keep strings alive across lines; capped at 10/publish oldest-first | `do={` with no closer → `Brace '{' opened here is never closed` |
| 7 | `unclosed-quote` | Error (1) | quoted string whose opening quote never terminates before EOF; error points at the OPENING quote, rest of document treated as string content (no cascade); same walk/cap as rule 6 | `:put "oops` → `Quoted string opened here is never closed` |

All diagnostics: `source="rsc-ls"`. Types: `Diagnostic{range, severity, code, source, message}` with `Range{Position{line,character}}`.

## Adding a New LSP Feature

1. **Choose handler**: add match arm in `Server::handle_message` (`main.rs`) (e.g., `textDocument/definition`, `textDocument/formatting`). Advertise in `initialize` capabilities.
2. **Reuse context**: call `build_before_cursor(doc, line, char)` → `parse_line(&data, &before)` → `LineContext{path, command, properties, last_token}`. For hover-like word precision, use `find_word_start/end` from `hover.rs`.
3. **Query MenuData**: `data.menu_by_path.get(&ctx.path)` for exact menu, `data.child_names_by_parent.get(&ctx.path)` for children/implicit parents, `data.menus` for prefix scans. Check `menu_type` (`Directory`/`Settings Directory`/`Command`) and `arg.required`/`arg_type`.
4. **Implement module**: create `lsp/src/<feature>.rs` (like `completion.rs`/`hover.rs`/`diagnostics.rs`/`symbols.rs`/`folding.rs`), expose a pure `compute_<feature>(...) -> Vec<T>/Option<Value>` function (no I/O) for testability. Register `mod <feature>` alongside the others in `main.rs`.
5. **Serialize LSP response**: `serde_json::json!({"jsonrpc":"2.0","id":id,"result":...})`; return `None` for notifications. Use `source="rsc-ls"` and correct `severity` for diagnostics.
6. **Wire publish if needed**: for push diagnostics pattern, add `publish_diagnostics` call after `didOpen`/`didChange`/`didClose`.
7. **Test**: add `#[cfg(test)]` with `MenuData::from_toml_str(synthetic)` (see the test modules in `diagnostics.rs`, `completion.rs`, `hover.rs`, `main.rs`) plus `MenuData::load()` real-data sanity. Run `cargo test -p rsc-ls`.
8. **WASM/extension**: if new `initializationOptions` or file types needed, update `extension.toml` and `src/lib.rs` (never use `std::env::var`/`cfg` there — use `current_platform`/`Worktree`).

## Performance & Limits

Canonical values live in `main.rs`; `server.rs` mirrors them and asserts equality in tests. Check the source if in doubt — do not trust this table blindly.

| Constant | Value | Defined in | Purpose |
|----------|-------|------------|---------|
| `MAX_MESSAGE_SIZE` | 10 MiB | `main.rs` | Drop oversized JSON-RPC bodies |
| `MAX_HEADER_SIZE` | 32 KiB | `main.rs` | Prevent header slowloris, drain+resync |
| `MAX_DOC_SIZE` | 5 MiB | `main.rs` | Truncate `didOpen`/`didChange` text at `floor_char_boundary` |
| `MAX_DOCS` | 100 | `main.rs` | Reject new URIs when cap reached |
| `MAX_DIAG_LINES` | 3000 | `diagnostics.rs` | Only first N lines diagnosed |
| `MAX_DIAG_BYTES` | 500 KB | `diagnostics.rs` | Truncate doc for diagnostics at char boundary |
| `MAX_SYNTAX_DIAGNOSTICS` | 10 | `diagnostics.rs` | Cap on unclosed/unmatched brace + quote diagnostics per publish (oldest-first) |
| `MAX_BRACE_DEPTH` | 4096 | `parser.rs` | Open-brace stack bound shared by folding + syntax diagnostics |

Incremental edits: `lsp_position_to_offset` + `apply_incremental_edit` (`main.rs`) handle `range` patches; on `InvalidRange`/`OutOfBounds` fall back to full replace. `floor_char_boundary` polyfill for UTF-8 safety. Large docs still publish diagnostics (capped) without OOM. URI validation helper: `server.rs` (`is_valid_file_uri`).

## Testing Strategy

```bash
cargo test -p rsc-ls                  # unit tests: menus, server, completion, hover, main, diagnostics
cargo test -p rsc-ls -- diagnostics   # single module
cargo test -p rsc-ls -- --nocapture   # show eprintln logs
python -m pytest tests/ -v            # Python integration tests
cd grammars/rsc && npx tree-sitter test  # grammar corpus
make validate                         # generate-check + fmt + clippy + test-all + sync-check + extract
```

| Area | File | Pattern |
|------|------|---------|
| Menus/indices | `lsp/src/menus.rs` | `test_*`, `synthetic` TOML via `from_toml_str`, `MenuData::load` real-data checks (`test_menus_are_not_empty`, `test_children_index_built`) |
| Tokenize/parse | `lsp/src/parser.rs` | `test_tokenize_*`, `test_build_before_cursor_*`, `test_parse_line_*`, `test_walk_structure_*` |
| Framing | `lsp/src/framing.rs` | golden streams: valid frames, garbage/malformed/duplicate headers terminal, oversized drained, EOF semantics |
| Encoding/patching | `lsp/src/encoding.rs` | `test_lsp_position_to_offset_*`, `test_apply_incremental_edit_*`, UTF-16 round-trips, CRLF |
| Caps/URI validation | `lsp/src/server.rs` | enclosure/capacity invariant tests, `is_valid_file_uri` sync with `crate::` |
| Completion | `lsp/src/completion.rs` | `test_root_*`, `test_submenu_*`, `test_arg_*`, `test_value_*`, `test_snippets_*` / `test_at_statement_start_gating` (B3), real-data `test_real_data_*` |
| Hover | `lsp/src/hover.rs` | `test_hover_menu_*`, `test_hover_property_*`, `test_hover_flag_*`, `test_hover_verb_*`, `test_find_word_*` |
| Diagnostics | `lsp/src/diagnostics.rs` | `test_unknown_menu`, `test_missing_required`, `test_duplicate`, `test_invalid_enum`, `test_large_doc_capped`, `test_implicit_parent` |
| Symbols/folding | `lsp/src/symbols.rs`, `lsp/src/folding.rs` | unit fixtures + server-level integration in `main.rs` (`test_document_symbols_*`, `test_folding_ranges_*`) incl. untracked-null and `-32602` cases |
| Manual E2E | Zed | `Install Dev Extension` → open `.rsc` → trigger `/` ` ` `=` completion, hover, folding gutter, outline/document symbols, verify `publishDiagnostics` |

Edge cases covered: empty files, quoted strings (incl. braces inside them), `[find]`, multi-menu docs, incremental edits, 5 MiB cap, UTF-8/UTF-16 positions (`héllo`, emoji), implicit parents, truncated `enum` in real data, unterminated braces/quotes at EOF.

## Debugging

Logs go to **stderr** (stdout reserved for JSON-RPC). View via `zed --foreground` or `zed: open log`.

Env: `RSC_LS_LOG` (preferred) or `RUST_LOG` fallback; values `error < warn < info < debug < trace` (also matches `rsc_ls=debug`, substring). Default `info`. Set `RSC_LS_LOG=debug` for verbose.

```bash
RSC_LS_LOG=debug cargo run -p rsc-ls          # local stdio
RSC_LS_LOG=trace zed --foreground             # Zed foreground logs
```

Prefixes: `[rsc-ls][ERROR]` / `[WARN]` / `[INFO]` / `[DEBUG]` / `[TRACE]` and `[mikrotik-zed]` for WASM extension (`src/lib.rs`). Startup logs menu count and limits. Non-`file://` URIs rejected (`didOpen`/`didChange`/`diagnostic`). `cfg!(test)` suppresses `publishDiagnostics` stdout during `cargo test`.

### Which binary is Zed running?

The extension resolves `rsc-ls` at runtime (PATH → cache → GitHub download), so a stale copy can hide behind a fresh one. Ask every candidate directly:

```bash
which -a rsc-ls | while read -r p; do printf '%s -> ' "$p"; "$p" --version; done
```

Every copy prints `rsc-ls <version>` (release builds append ` (build <sha7>)` via the compile-time `RSC_LS_BUILD_SHA`) and exits 0 without reading stdin or loading menus; unknown flags exit 2 with usage on stderr (`--help`/`-h` prints it on stdout). The startup stderr line — visible via `zed: open log` — also carries version + pid, e.g. `[rsc-ls][INFO] rsc-ls 0.1.5 starting (pid=1234, …)`, so multiple instances are correlatable in logs.

## Build

```bash
cargo build -p rsc-ls --release                          # → target/release/rsc-ls
cargo build --target wasm32-wasip2 --release             # WASM component (Zed builds via extension_builder)
cargo test -p rsc-ls && python -m pytest tests/ -v
```

Auto-download: `RscExtension::language_server_command` (`src/lib.rs`) tries `worktree.which("rsc-ls")` → cached `rsc-ls` → `current_platform()` → `latest_github_release` / `github_release_by_tag_name` → `download_file` → `make_file_executable`. Triples: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`; Windows → error with manual build hint.

## Reference

- `zed_extension_api 0.7`: https://docs.rs/crate/zed_extension_api/latest
- Zed LSP docs: https://zed.dev/docs/extensions/languages
- RouterOS CLI: https://manual.mikrotik.com/docs/cli-reference/ — truth `llms-full.txt` → `data/commands.toml` via `scripts/extract_commands.py`
