// ── MikroTik RouterOS Script Language Server ─────────────────────
//
// LSP over stdio, implemented in pure Rust.  Commands.toml is
// embedded at compile time — no external files needed.
//
// LSP handlers:
//   textDocument/completion – menu path, command verb, and property suggestions
//   textDocument/hover        – description for commands and properties
//
// Protocol notes:
// - Uses Content-Length framing with JSON-RPC 2.0 over stdio.
// - Advertises textDocumentSync = { openClose: true, change: 2 } (Incremental).
//   Clients send range-scoped edits; for robustness full-text replacements
//   (no `range`) are still handled, and a failed incremental patch falls
//   back to a full document replace.
// - Content-Length is capped at 10 MiB to avoid unbounded allocation.

mod caps;
mod cli;
mod completion;
mod diagnostics;
mod encoding;
mod folding;
mod framing;
mod hover;
mod live;
mod logging;
mod menus;
mod navigation;
mod parser;
mod server;
mod signature;
mod suggest;
mod symbols;

pub(crate) use caps::{
    MAX_DIAG_BYTES, MAX_DIAG_LINES, MAX_DOC_SIZE, MAX_DOCS, MAX_HEADER_SIZE, MAX_MESSAGE_SIZE,
};
pub(crate) use encoding::{convert_position, floor_char_boundary};
pub(crate) use logging::{log_debug, log_error, log_info, log_level, log_warn};
pub(crate) use parser::{
    MAX_BRACE_DEPTH, StructureEvent, build_before_cursor, parse_line, tokenize_with_spans,
    walk_structure,
};
pub(crate) use server::Server;

// The re-exports below exist only so the child-of-root test modules keep
// resolving moved/shared items through `use super::*`; their production
// consumers import from the defining modules directly, so compiling them
// into the non-test build would trip `unused_imports`.
#[cfg(test)]
pub(crate) use caps::MAX_CODE_ACTIONS;
#[cfg(test)]
pub(crate) use encoding::PositionEncoding;
#[cfg(test)]
pub(crate) use server::{exit_code, is_valid_file_uri};

use menus::MenuData;

// Resource caps live in caps.rs — single source of truth; re-exported here
// so existing paths keep working.

fn main() {
    // CLI flags are handled FIRST — before logging, env reads, or data
    // loading — so `--version`/`--help` probes stay side-effect-free:
    // they never read stdin (must not hang) and never parse menus.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = cli::run_cli_command(cli::parse_cli_args(&args)) {
        std::process::exit(code);
    }

    // Initialize log level early (reads RSC_LS_LOG)
    let level = log_level();
    // Startup banner carries version + pid so multiple server instances are
    // correlatable in `zed: open log`; the version token matches the output
    // of `rsc-ls --version` exactly.
    eprintln!(
        "[rsc-ls][INFO] {} starting (pid={}, RSC_LS_LOG={:?} -> {:?})",
        cli::version_string(),
        std::process::id(),
        std::env::var("RSC_LS_LOG").unwrap_or_else(|_| "(unset, default info)".to_string()),
        level
    );
    // Hint for observability: when run via Zed, use `zed --foreground` to see these logs,
    // or `zed: open log` action. Setting `RSC_LS_LOG=debug` enables verbose diagnostics.
    let data = MenuData::load();
    log_info!("language server started, {} menus loaded", data.menus.len());
    log_debug!(
        "limits: MAX_MESSAGE_SIZE={} MAX_HEADER_SIZE={} MAX_DOC_SIZE={} MAX_DOCS={}",
        MAX_MESSAGE_SIZE,
        MAX_HEADER_SIZE,
        MAX_DOC_SIZE,
        MAX_DOCS
    );

    // Live device data (opt-in, TTL-scoped, in-memory only).
    let live_config = live::LiveConfig::from_env();
    live_config.log_status();
    let live_cache =
        std::sync::Arc::new(std::sync::Mutex::new(live::LiveCache::with_default_ttl()));

    let mut server = Server::new_with_live(data, live_config, live_cache);
    server.run();
    log_info!("language server exiting");
}

// ── Server core lives in server.rs ─────────────────────────────
//
// Server, dispatch loop, document store and URI validation live in
// server.rs; re-exported above next to the other shared-module paths
// so every existing crate-root reference keeps resolving unchanged.

// Unit/integration-surface tests extracted verbatim from this file (pure move).
// They remain child-of-root modules: `use super::*` reaches everything still
// declared here directly, and the moved Server machinery through the root
// re-exports above.
#[cfg(test)]
#[path = "main_tests/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "main_tests/extra_main_coverage.rs"]
mod extra_main_coverage;

#[cfg(test)]
#[path = "main_tests/position_encoding.rs"]
mod position_encoding;

#[cfg(test)]
#[path = "main_tests/signature_help.rs"]
mod signature_help;
