// ── Structured logging (RSC_LS_LOG) ──────────────────────────────────────
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

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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

impl LogLevel {
    /// Lowercase level token for log lines (`info`, not `Info`).
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

// ── Session clock ────────────────────────────────────────────────────────
//
// Every log line carries a monotonic `[T+0.042s]` tag (elapsed since
// process start): compact, timezone-free, and complementary to the
// per-request `latency=…ms` suffix. Zed shows server stderr as plain
// text without timestamps of its own, so absolute wall-clock appears
// only once, in the startup banner ([`utc_rfc3339_now`]).

static START: OnceLock<Instant> = OnceLock::new();

fn start() -> &'static Instant {
    START.get_or_init(Instant::now)
}

/// Monotonic elapsed tag for log lines, e.g. `[T+0.042s]`.
pub(crate) fn elapsed_tag() -> String {
    format!("[T+{:.3}s]", start().elapsed().as_secs_f64())
}

/// Current UTC time as `2026-09-04T15:30:01Z` (RFC3339, seconds precision).
///
/// Dependency-free days-to-civil conversion (Hinnant algorithm) — no
/// `chrono`/`time` crate for a single timestamp. Falls back to the epoch
/// on clock error.
pub(crate) fn utc_rfc3339_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_utc_rfc3339(secs)
}

pub(crate) fn format_utc_rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// Proleptic Gregorian date for days since 1970-01-01 (Hinnant algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Short hash for URI (8 hex, SipHash) — never log raw URI.
pub(crate) fn uri_hash(uri: &str) -> String {
    let mut h = DefaultHasher::new();
    uri.hash(&mut h);
    format!("{:016x}", h.finish())[..8].to_string()
}

/// Bounded, non-reversible URI descriptor for log lines.
///
/// A document URI is client-controlled and can be up to `MAX_MESSAGE_SIZE`
/// (multi-MiB). Log only its byte length and short hash so a malformed URI
/// cannot flood the log or expose path material. Use this everywhere a URI
/// would otherwise be interpolated raw.
pub(crate) fn uri_for_log(uri: &str) -> String {
    format!("len={} hash={}", uri.len(), uri_hash(uri))
}

/// Sanitize a JSON-RPC method token for a log line.
///
/// Strips every ASCII control character (including `\r`/`\n`, so a crafted
/// `method` cannot forge log lines) and caps the result at 64 chars.
/// `request_suffix` must use this rather than interpolating `method` raw.
pub(crate) fn sanitize_method_for_log(m: &str) -> String {
    let stripped: String = m.chars().filter(|c| !c.is_control()).collect();
    if stripped.chars().count() > 64 {
        stripped.chars().take(64).collect()
    } else {
        stripped
    }
}

pub(crate) fn request_suffix(m: &str, uri: Option<&str>, d: u64, enc: &str) -> String {
    let h = uri.map(uri_hash).unwrap_or_else(|| "none".to_string());
    format!(
        "method={} uri_hash={h} latency={d}ms encoding={enc}",
        sanitize_method_for_log(m)
    )
}

/// Sanitize a value before interpolating it into a log line.
///
/// Strips `\r` and `\n` (log-injection defense) and truncates to 128
/// chars. Apply to every host/user/url/path interpolation in
/// `log_info!`/`log_warn!`/`log_debug!` paths. Never applied to `pass`.
pub(crate) fn sanitize_for_log(s: &str) -> String {
    let stripped: String = s.chars().filter(|c| *c != '\r' && *c != '\n').collect();
    if stripped.chars().count() > 128 {
        stripped.chars().take(128).collect()
    } else {
        stripped
    }
}

/// Truncate a command name for log lines (256-char cap, no newline).
pub(crate) fn truncate_command_for_log(s: &str) -> String {
    let stripped: String = s.chars().filter(|c| *c != '\r' && *c != '\n').collect();
    if stripped.chars().count() > 256 {
        stripped.chars().take(256).collect()
    } else {
        stripped
    }
}

/// Redact credential material from log/error text (mirrors
/// `scripts/_mikrotik_shared.py::redact_secrets`).
///
/// Replaces the password, the base64 of `user:password` (HTTP Basic), and
/// the base64 of the password alone with `[REDACTED]`. Never returns
/// credential material; safe to apply unconditionally before logging or
/// embedding in `LiveError::Network`.
pub(crate) fn redact_secrets(text: &str, pass: &str, user: &str) -> String {
    use base64::Engine;
    let mut out = text.to_string();
    if pass.is_empty() {
        return out;
    }
    let mut secrets = vec![pass.to_string()];
    let creds = format!("{user}:{pass}");
    secrets.push(base64::engine::general_purpose::STANDARD.encode(creds.as_bytes()));
    secrets.push(base64::engine::general_purpose::STANDARD.encode(pass.as_bytes()));
    for secret in secrets {
        if !secret.is_empty() && out.contains(&secret) {
            out = out.replace(&secret, "[REDACTED]");
        }
    }
    out
}

macro_rules! log_error {
    ($($arg:tt)*) => {
        if $crate::logging::should_log($crate::logging::LogLevel::Error) {
            eprintln!("[rsc-ls][ERROR]{} {}", $crate::logging::elapsed_tag(), format!($($arg)*));
        }
    };
}

macro_rules! log_warn {
    ($($arg:tt)*) => {
        if $crate::logging::should_log($crate::logging::LogLevel::Warn) {
            eprintln!("[rsc-ls][WARN]{} {}", $crate::logging::elapsed_tag(), format!($($arg)*));
        }
    };
}

macro_rules! log_info {
    ($($arg:tt)*) => {
        if $crate::logging::should_log($crate::logging::LogLevel::Info) {
            eprintln!("[rsc-ls][INFO]{} {}", $crate::logging::elapsed_tag(), format!($($arg)*));
        }
    };
}

macro_rules! log_debug {
    ($($arg:tt)*) => {
        if $crate::logging::should_log($crate::logging::LogLevel::Debug) {
            eprintln!("[rsc-ls][DEBUG]{} {}", $crate::logging::elapsed_tag(), format!($($arg)*));
        }
    };
}

// Trace handling note: `log_level` still parses "trace" (mapping it to the
// most verbose supported level, [`LogLevel::Trace`]), but the `log_trace!`
// macro itself was deleted as dead machinery — it had no call sites. If a
// trace-level emitter ever appears, re-add the macro here together with its
// first caller and export it in the list below.
pub(crate) use {log_debug, log_error, log_info, log_warn};
