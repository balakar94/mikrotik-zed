// ── Structured logging (RSC_LS_LOG) ─────────────────────────────
//
// Visible via `zed --foreground` or `zed: open log`.
// Levels: error < warn < info < debug < trace
// Env var: RSC_LS_LOG (e.g. "debug", "info", "trace", "warn", "error")
// Also respects RUST_LOG as fallback. Default: info.
// All logs go to stderr so they appear in Zed's foreground logs without
// corrupting LSP stdio (stdout is reserved for JSON-RPC).
//
// The macros live here (not in main.rs) so any module can use them: their
// bodies reference [`should_log`] through the absolute `crate::logging::`
// path, which resolves correctly at every expansion site inside this crate.

use std::sync::OnceLock;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum LogLevel {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

static LOG_LEVEL: OnceLock<LogLevel> = OnceLock::new();

pub(crate) fn log_level() -> LogLevel {
    *LOG_LEVEL.get_or_init(|| {
        let raw = std::env::var("RSC_LS_LOG")
            .or_else(|_| std::env::var("RUST_LOG"))
            .unwrap_or_else(|_| "info".to_string());
        match raw.to_ascii_lowercase().trim() {
            "error" => LogLevel::Error,
            "warn" | "warning" => LogLevel::Warn,
            "info" => LogLevel::Info,
            "debug" => LogLevel::Debug,
            "trace" => LogLevel::Trace,
            // Support RUST_LOG style like "rsc_ls=debug" or "debug"
            s if s.contains("trace") => LogLevel::Trace,
            s if s.contains("debug") => LogLevel::Debug,
            s if s.contains("info") => LogLevel::Info,
            s if s.contains("warn") => LogLevel::Warn,
            _ => LogLevel::Info,
        }
    })
}

pub(crate) fn should_log(level: LogLevel) -> bool {
    level <= log_level()
}

macro_rules! log_error {
    ($($arg:tt)*) => {
        if $crate::logging::should_log($crate::logging::LogLevel::Error) {
            eprintln!("[rsc-ls][ERROR] {}", format!($($arg)*));
        }
    };
}

macro_rules! log_warn {
    ($($arg:tt)*) => {
        if $crate::logging::should_log($crate::logging::LogLevel::Warn) {
            eprintln!("[rsc-ls][WARN] {}", format!($($arg)*));
        }
    };
}

macro_rules! log_info {
    ($($arg:tt)*) => {
        if $crate::logging::should_log($crate::logging::LogLevel::Info) {
            eprintln!("[rsc-ls][INFO] {}", format!($($arg)*));
        }
    };
}

macro_rules! log_debug {
    ($($arg:tt)*) => {
        if $crate::logging::should_log($crate::logging::LogLevel::Debug) {
            eprintln!("[rsc-ls][DEBUG] {}", format!($($arg)*));
        }
    };
}

// Trace handling note: `log_level` still parses "trace" (mapping it to the
// most verbose supported level, [`LogLevel::Trace`]), but the `log_trace!`
// macro itself was deleted as dead machinery — it had no call sites. If a
// trace-level emitter ever appears, re-add the macro here together with its
// first caller and export it in the list below.
pub(crate) use {log_debug, log_error, log_info, log_warn};
