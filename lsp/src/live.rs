// ── Live device data (opt-in, in-memory only) ───────────────────
//
// Provides interface-name enrichment for completion without ever touching
// the snapshot `data/commands.toml` on disk. All state lives in a
// TTL-scoped, capped in-memory cache (never committed, never overwrites
// the file). Opt-in via `RSC_LS_LIVE=1` or `MIKROTIK_LIVE=1` and the
// companion env vars mirrored from `scripts/mikrotik-deploy.py`.
//
// Defensive invariants (hard rule #7):
// - No filesystem access beyond the process env.
// - Response bytes, item counts, and value lengths are capped (see `caps.rs`).
// - Host and live values are allow-list filtered; control chars / nulls
//   are rejected.
// - `LiveConfig` never logs `pass`.
//
// Network notes:
// - LSP is a native binary and MAY use std env / threads / networking
//   (the `wasm32-wasip2` restriction applies only to the shim at `src/lib.rs`).
// - Fetch uses `ureq` with a short per-request timeout and basic auth.
// - Completion never blocks more than `LIVE_FETCH_BLOCKING_TIMEOUT_SECS`.

use crate::caps::{
    LIVE_CUSTOM_RESOURCES_MAX, LIVE_FETCH_BLOCKING_TIMEOUT_SECS, LIVE_MAX_HOSTS,
    LIVE_NEGATIVE_TTL_SECS, LIVE_TIMEOUT_SECS, LIVE_TTL_SECS, MAX_CACHE_ENTRIES, MAX_LIVE_ITEMS,
    MAX_LIVE_RESPONSE_BYTES, MAX_LIVE_VALUE_LEN,
};
use crate::logging::{log_debug, log_info, log_warn, redact_secrets, sanitize_for_log};
use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

// ── Bounded fetch concurrency (A-03) ───────────────────────────────
//
// `trigger_background_fetch` historically spawned one thread per cache
// miss, coalesced only by the 2 s window (`LIVE_FETCH_BLOCKING_TIMEOUT_SECS`).
// Under flapping completions that still allowed bursts. Cap concurrency
// to 2 threads globally via an `AtomicUsize` semaphore — smallest safe
// change, no new dependency, preserves the existing coalescing window.
static ACTIVE_FETCHES: AtomicUsize = AtomicUsize::new(0);
const MAX_CONCURRENT_FETCHES: usize = 2;

fn try_acquire_fetch_permit() -> bool {
    let mut cur = ACTIVE_FETCHES.load(Ordering::Acquire);
    loop {
        if cur >= MAX_CONCURRENT_FETCHES {
            return false;
        }
        match ACTIVE_FETCHES.compare_exchange(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(next) => cur = next,
        }
    }
}

fn release_fetch_permit() {
    ACTIVE_FETCHES.fetch_sub(1, Ordering::AcqRel);
}

struct FetchPermitGuard;
impl Drop for FetchPermitGuard {
    fn drop(&mut self) {
        release_fetch_permit();
    }
}

// ── CustomResource ───────────────────────────────────────────────

/// User-defined live resource mapping via `RSC_LS_LIVE_RESOURCES`.
///
/// JSON shape: `{ "property": "packet-mark", "path": "/rest/ip/firewall/mangle", "field": "new-packet-mark" }`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomResource {
    /// Property name that triggers this resource (e.g. "packet-mark").
    pub property: String,
    /// REST path on the device (e.g. "/rest/ip/firewall/mangle").
    pub path: String,
    /// JSON field to extract from each array entry (e.g. "new-packet-mark").
    pub field: String,
}

// ── LiveConfig ───────────────────────────────────────────────────

/// Live device connection configuration, parsed from the environment.
///
/// Mirrors `scripts/mikrotik-deploy.py` semantics so the same env vars
/// work for both the deploy companion and the language server.
#[derive(Clone)]
pub struct LiveConfig {
    /// Opt-in flag: `RSC_LS_LIVE=1` or `MIKROTIK_LIVE=1`.
    pub enabled: bool,
    /// Device host/IP (`MIKROTIK_HOST`). Empty when not set. Primary host for backward compat.
    pub host: String,
    /// All hosts when `MIKROTIK_HOST` is comma-separated (first is primary). Capped to `LIVE_MAX_HOSTS`.
    /// Multi-host is validated but only the primary host is currently fetched; additional hosts retained for future use.
    pub hosts: Vec<String>,
    /// Username (`MIKROTIK_USER`, default `admin`).
    pub user: String,
    /// Password (`MIKROTIK_PASS`). Never logged.
    pub pass: String,
    /// REST port (`MIKROTIK_PORT`, default `443` for REST).
    pub port: u16,
    /// Whether to verify TLS certificates (`MIKROTIK_SSL=0` => false).
    pub ssl_verify: bool,
    /// Whether to force plain HTTP (`MIKROTIK_HTTP=1` => true).
    pub force_http: bool,
    /// Per-request timeout in seconds (clamped 1..30, default 5).
    pub timeout_secs: u64,
    /// User-defined custom live resources (capped to `LIVE_CUSTOM_RESOURCES_MAX`).
    pub custom_resources: Vec<CustomResource>,
    /// Whether loopback/private hosts are allowed (`RSC_LS_LIVE_ALLOW_LOOPBACK=1`).
    /// Default deny (false) — when false, `127.0.0.0/8`, `::1`, `10/8`, `192.168/16` etc are rejected via `is_loopback_or_private`.
    pub allow_loopback: bool,
    /// SPKI SHA256 pin (`MIKROTIK_FINGERPRINT=sha256:<hex>`). When set and
    /// valid, TLS uses a pinning verifier instead of disabling verification.
    /// `None` when unset or unparsable (see `fingerprint_invalid`).
    pub fingerprint: Option<[u8; 32]>,
    /// True when `MIKROTIK_FINGERPRINT` was present but failed to parse.
    /// Fail-closed: `is_active()` returns false while this is set.
    pub fingerprint_invalid: bool,
    /// Custom CA bundle path (`MIKROTIK_CA_FILE`). Empty when unset.
    /// Loaded at fetch time; load failures are fail-closed (`LiveError::Network`).
    pub ca_file: String,
}

impl std::fmt::Debug for LiveConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveConfig")
            .field("enabled", &self.enabled)
            .field("host", &self.host)
            .field("hosts", &self.hosts)
            .field("user", &self.user)
            .field("pass", &"[REDACTED]")
            .field("port", &self.port)
            .field("ssl_verify", &self.ssl_verify)
            .field("ssl_verify_effective", &self.ssl_verify_effective())
            .field("force_http", &self.force_http)
            .field("timeout_secs", &self.timeout_secs)
            .field("custom_resources", &self.custom_resources)
            .field("allow_loopback", &self.allow_loopback)
            .field("fingerprint_set", &self.fingerprint.is_some())
            .field("fingerprint_invalid", &self.fingerprint_invalid)
            .field("ca_file", &sanitize_for_log(&self.ca_file))
            .finish()
    }
}

/// Validate a device username.
///
/// Allows 1..64 chars matching `^[A-Za-z0-9._-]+$`. Rejects `:`, control
/// chars, `@`, `%`, null, and newlines (all implicitly excluded by the
/// allowlist, checked explicitly for clear rejection). Returns `None` when
/// invalid; callers fall back to `admin` with a WARN.
pub(crate) fn validate_user(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 64 {
        return None;
    }
    if trimmed.contains('\0') || trimmed.chars().any(|c| c.is_control()) {
        return None;
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return None;
    }
    Some(trimmed.to_string())
}

/// Whether workspace settings may override transport-security keys.
///
/// Opt-in via `RSC_LS_ALLOW_SETTINGS_TRANSPORT=1`. When false (default),
/// `host`/`user` (F2), `ssl_verify=false`, `force_http=true`,
/// `allow_loopback=true`, and `custom_resources` from settings are ignored
/// (env values always win).
fn settings_transport_allowed() -> bool {
    std::env::var("RSC_LS_ALLOW_SETTINGS_TRANSPORT")
        .ok()
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

#[cfg(test)]
pub(crate) static SETTINGS_TRANSPORT_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Test-only: run `f` with `RSC_LS_ALLOW_SETTINGS_TRANSPORT` set/removed.
///
/// Serialized by a process-wide mutex so parallel `cargo test` threads
/// cannot observe a half-applied flag. Restores the previous value
/// afterwards. Production code never calls this.
#[cfg(test)]
pub(crate) fn with_settings_transport_env<R>(allowed: bool, f: impl FnOnce() -> R) -> R {
    let lock = SETTINGS_TRANSPORT_TEST_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let key = "RSC_LS_ALLOW_SETTINGS_TRANSPORT";
    let prev = std::env::var(key).ok();
    // SAFETY: test-only helper, serialized by the mutex above; no other
    // thread observes the interim value. (`std::env::set_var` is `unsafe`
    // in edition 2024.)
    if allowed {
        unsafe { std::env::set_var(key, "1") };
    } else {
        unsafe { std::env::remove_var(key) };
    }
    let out = f();
    match prev {
        Some(v) => unsafe { std::env::set_var(key, v) },
        None => unsafe { std::env::remove_var(key) },
    }
    out
}

impl LiveConfig {
    /// Read the live config from the current process environment.
    pub fn from_env() -> Self {
        Self::from_env_with(|k| std::env::var(k).ok())
    }

    /// Test-friendly constructor: `get` supplies env values (e.g. from a map).
    ///
    /// When `get` returns `None`, the variable is treated as unset.
    pub(crate) fn from_env_with<F>(get: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let enabled = get("RSC_LS_LIVE")
            .as_deref()
            .map(|v| v.trim() == "1")
            .unwrap_or(false)
            || get("MIKROTIK_LIVE")
                .as_deref()
                .map(|v| v.trim() == "1")
                .unwrap_or(false);

        let host_raw = get("MIKROTIK_HOST").unwrap_or_default();
        let hosts = parse_hosts(&host_raw);
        let host = hosts.first().cloned().unwrap_or_default();
        if hosts.len() > 1 {
            log_info!(
                "live multi-host {} (primary={})",
                hosts.len(),
                sanitize_for_log(&host)
            );
        }

        let user_raw = get("MIKROTIK_USER").unwrap_or_default();
        let user = if user_raw.trim().is_empty() {
            "admin".to_string()
        } else {
            match validate_user(&user_raw) {
                Some(u) => u,
                None => {
                    log_warn!(
                        "invalid MIKROTIK_USER={:?}, falling back to admin",
                        sanitize_for_log(user_raw.trim())
                    );
                    "admin".to_string()
                }
            }
        };
        let pass = get("MIKROTIK_PASS").unwrap_or_default();

        // Port: mirror _mikrotik_shared.py::env_int with default 443 and warning on bad input.
        let port_raw = get("MIKROTIK_PORT");
        let port = parse_env_u16(&port_raw, 443, 1, 65535, "MIKROTIK_PORT");

        // SSL verify: MIKROTIK_SSL=0 => false, otherwise true (verify only).
        let ssl_verify = !matches!(get("MIKROTIK_SSL").as_deref().map(|s| s.trim()), Some("0"));
        let force_http = get("MIKROTIK_HTTP")
            .as_deref()
            .map(|v| v.trim() == "1")
            .unwrap_or(false);

        // Timeout: default 5s for live, clamped 1..30.
        let timeout_raw = get("MIKROTIK_TIMEOUT");
        let timeout_parsed = parse_env_u64(&timeout_raw, LIVE_TIMEOUT_SECS, "MIKROTIK_TIMEOUT");
        let timeout_secs = timeout_parsed.clamp(1, 30);

        // Custom resources from env JSON.
        let custom_raw = get("RSC_LS_LIVE_RESOURCES").or_else(|| get("MIKROTIK_LIVE_RESOURCES"));
        let custom_resources = parse_custom_resources(custom_raw.as_deref());

        let allow_loopback = get("RSC_LS_LIVE_ALLOW_LOOPBACK")
            .as_deref()
            .map(|v| v.trim() == "1")
            .unwrap_or(false);

        // TLS pin + custom CA. Fingerprint format:
        // `MIKROTIK_FINGERPRINT=sha256:<64 hex>`. Invalid values are
        // fail-closed via `fingerprint_invalid` (see `is_active`).
        let fingerprint_raw = get("MIKROTIK_FINGERPRINT");
        let (fingerprint, fingerprint_invalid) = parse_fingerprint(fingerprint_raw.as_deref());
        let ca_file = get("MIKROTIK_CA_FILE")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        LiveConfig {
            enabled,
            host,
            hosts,
            user,
            pass,
            port,
            ssl_verify,
            force_http,
            timeout_secs,
            custom_resources,
            allow_loopback,
            fingerprint,
            fingerprint_invalid,
            ca_file,
        }
    }

    /// Whether live fetching is active.
    ///
    /// Requires opt-in `enabled` AND non-empty `host` + `pass` with valid host.
    /// A present-but-invalid `MIKROTIK_FINGERPRINT` is fail-closed (inactive).
    pub fn is_active(&self) -> bool {
        if self.fingerprint_invalid {
            return false;
        }
        self.enabled
            && !self.host.is_empty()
            && !self.pass.is_empty()
            && validate_host_with_allow(&self.host, self.allow_loopback).is_ok()
            && self.port != 0
    }

    /// Whether TLS verification is effectively enabled for the current scheme.
    ///
    /// Verification only matters when the resolved scheme is `https`; on `http`
    /// the flag is irrelevant and effective is `false`. A valid SPKI pin or a
    /// custom CA bundle counts as verification even when `MIKROTIK_SSL=0`,
    /// because the pinning/custom-CA verifier still authenticates the device
    /// instead of disabling checks.
    pub fn ssl_verify_effective(&self) -> bool {
        if self.scheme() != "https" {
            return false;
        }
        self.ssl_verify || self.fingerprint.is_some() || !self.ca_file.is_empty()
    }

    /// Resolve the REST scheme, mirroring `scripts/_mikrotik_shared.py::resolve_scheme`.
    ///
    /// Default is HTTPS; plain HTTP requires explicit `force_http`.
    /// Legacy shim: `--no-ssl-verify` (here `!ssl_verify`) on a non-standard
    /// port outside 443/8729 historically forced `http`; the live client
    /// preserves that observable behaviour without emitting the deploy warning.
    pub fn scheme(&self) -> &'static str {
        resolve_scheme(self.port, self.force_http, self.ssl_verify)
    }

    /// Log whether live is enabled or disabled (never logs `pass`).
    pub fn log_status(&self) {
        if self.is_active() {
            // Host is safe to log (no pass); port and scheme are non-sensitive.
            // Host/user values go through sanitize_for_log (strip CR/LF, 128 cap).
            // Fingerprint bytes are a public-key hash (not a secret) but only
            // the presence flag is logged to keep the line bounded.
            log_info!(
                "live enabled host={} port={} scheme={} user={} ssl_verify={} ssl_verify_effective={} timeout={}s hosts={:?} custom_resources={} allow_loopback={} fingerprint_set={} ca_file_set={}",
                sanitize_for_log(&self.host),
                self.port,
                self.scheme(),
                sanitize_for_log(&self.user),
                self.ssl_verify,
                self.ssl_verify_effective(),
                self.timeout_secs,
                sanitize_for_log(&format!("{:?}", self.hosts)),
                self.custom_resources.len(),
                self.allow_loopback,
                self.fingerprint.is_some(),
                !self.ca_file.is_empty()
            );
            if self.hosts.len() > 1 {
                log_info!(
                    "live multi-host active count={} primary={}",
                    self.hosts.len(),
                    sanitize_for_log(&self.host)
                );
            }
        } else if self.enabled {
            // Opt-in was requested but required vars missing/invalid.
            log_info!(
                "live enabled but inactive — missing/invalid MIKROTIK_HOST or MIKROTIK_PASS (opt-in via RSC_LS_LIVE=1)"
            );
        } else {
            log_info!("live disabled (opt-in via RSC_LS_LIVE=1 or MIKROTIK_LIVE=1)");
        }
    }

    /// Build a `LiveConfig` by overlaying `settings` JSON on top of `from_env()`.
    ///
    /// Supports both env-style keys (`MIKROTIK_HOST`) and lower-case keys
    /// (`host`, `port`, ...), and nesting under `rsc.live` or `mikrotik`.
    /// Used for hot-reload via `workspace/didChangeConfiguration`.
    pub fn from_settings_value(v: &serde_json::Value) -> Self {
        let mut cfg = Self::from_env();
        Self::apply_settings_value(&mut cfg, v);
        cfg
    }

    /// Apply settings overlay to an existing config (mutates in place).
    ///
    /// Only objects under an explicit scope (`rsc.live`, `rsc`, `mikrotik`,
    /// optionally nested under `settings`) are honored. A bare object that
    /// merely happens to contain a host-like key is IGNORED: accepting it
    /// would let unrelated editor settings hijack the device connection.
    /// `enabled` is never settings-overridable (env opt-in only).
    ///
    /// Transport-security keys (`ssl_verify=false`, `force_http=true`,
    /// `allow_loopback=true`, `custom_resources`) are privileged: they are
    /// ignored from workspace settings unless
    /// `RSC_LS_ALLOW_SETTINGS_TRANSPORT=1` is set. Env values always win.
    pub fn apply_settings_value(cfg: &mut Self, v: &serde_json::Value) {
        Self::apply_settings_value_with_transport(cfg, v, settings_transport_allowed());
    }

    /// Test-friendly overlay with explicit transport opt-in flag.
    pub(crate) fn apply_settings_value_with_transport(
        cfg: &mut Self,
        v: &serde_json::Value,
        allow_transport: bool,
    ) {
        // Find the most relevant settings object; without an explicit scope
        // there is nothing to overlay.
        let Some(settings_obj) = find_settings_object(v) else {
            log_debug!("live settings overlay ignored: no rsc.live/mikrotik scope");
            return;
        };

        let prev_host = cfg.host.clone();
        let prev_hosts = cfg.hosts.clone();
        if let Some(host_val) = get_settings_str(settings_obj, &["host", "MIKROTIK_HOST"]) {
            let hosts = parse_hosts(&host_val);
            if hosts.is_empty() {
                // Nothing valid to apply; fall through silently.
            } else if hosts[0] != cfg.host || hosts != cfg.hosts {
                // F2: workspace settings may not redirect the device target
                // (host/hosts) unless RSC_LS_ALLOW_SETTINGS_TRANSPORT=1.
                // Default deny: credentials would be sent to the new host.
                if !allow_transport {
                    log_warn!(
                        "live settings host ignored (was {:?} now {:?}): set RSC_LS_ALLOW_SETTINGS_TRANSPORT=1 to allow workspace transport overrides",
                        sanitize_for_log(&cfg.host),
                        sanitize_for_log(&hosts[0])
                    );
                } else {
                    cfg.host = hosts[0].clone();
                    cfg.hosts = hosts;
                }
            }
        }
        // SECURITY: workspace settings can redirect credentials. Warn loudly
        // whenever the overlay actually changes the effective live target so
        // the user notices a malicious `.zed/settings.json` redirect. The
        // overlay still applies (warn, not block); `pass` stays env-only.
        // Host changes via settings additionally require
        // RSC_LS_ALLOW_SETTINGS_TRANSPORT=1 (gated above); this WARN covers
        // the allowed case only.
        if cfg.host != prev_host || cfg.hosts != prev_hosts {
            log_warn!(
                "live target host came from workspace settings (was {:?} now {:?} hosts {:?}): credentials will be sent there — verify the host is trusted",
                sanitize_for_log(&prev_host),
                sanitize_for_log(&cfg.host),
                sanitize_for_log(&format!("{:?}", cfg.hosts))
            );
        }
        if let Some(user_val) =
            get_settings_str(settings_obj, &["user", "username", "MIKROTIK_USER"])
        {
            let trimmed = user_val.trim();
            if !trimmed.is_empty() {
                // F2: user/username from settings is a transport-identity
                // change (selects which credentials are sent). Default deny
                // unless RSC_LS_ALLOW_SETTINGS_TRANSPORT=1.
                if trimmed != cfg.user && !allow_transport {
                    log_warn!(
                        "live settings user ignored (was {:?} now {:?}): set RSC_LS_ALLOW_SETTINGS_TRANSPORT=1 to allow workspace transport overrides",
                        sanitize_for_log(&cfg.user),
                        sanitize_for_log(trimmed)
                    );
                } else {
                    match validate_user(&user_val) {
                        Some(u) => cfg.user = u,
                        None => {
                            log_warn!(
                                "invalid live user from workspace settings (was {:?} now {:?}), falling back to admin",
                                sanitize_for_log(&cfg.user),
                                sanitize_for_log(trimmed)
                            );
                            cfg.user = "admin".to_string();
                        }
                    }
                }
            }
        }
        // SECURITY: secrets in workspace settings are ignored (warn-only).
        // `MIKROTIK_PASS` must come from env/keychain — settings files can be
        // committed to shared repos, so they are never a password source.
        if get_settings_str(settings_obj, &["pass", "password", "MIKROTIK_PASS"]).is_some() {
            log_warn!(
                "live settings pass/password ignored: never store MIKROTIK_PASS in workspace settings; use env/keychain instead"
            );
        }
        if let Some(port_val) = get_settings_port(settings_obj) {
            cfg.port = port_val;
        }
        // Privileged transport keys: downgrades via settings require opt-in.
        // Env values always win when the opt-in is absent.
        let requested_ssl: Option<bool> = if let Some(b) = get_settings_bool(
            settings_obj,
            &["ssl_verify", "ssl", "MIKROTIK_SSL", "verify_ssl"],
        ) {
            Some(b)
        } else if let Some(s) = get_settings_str(settings_obj, &["MIKROTIK_SSL"]) {
            let trimmed = s.trim();
            if trimmed == "0" {
                Some(false)
            } else if trimmed == "1" {
                Some(true)
            } else {
                None
            }
        } else {
            None
        };
        if let Some(ssl_val) = requested_ssl {
            let is_downgrade = !ssl_val && cfg.ssl_verify;
            if is_downgrade && !allow_transport {
                log_warn!(
                    "live settings ssl_verify downgrade ignored (was {:?} now {:?}): set RSC_LS_ALLOW_SETTINGS_TRANSPORT=1 to allow workspace transport overrides",
                    cfg.ssl_verify,
                    ssl_val
                );
            } else {
                cfg.ssl_verify = ssl_val;
            }
        }
        if let Some(http_val) =
            get_settings_bool(settings_obj, &["force_http", "http", "MIKROTIK_HTTP"])
        {
            let is_downgrade = http_val && !cfg.force_http;
            if is_downgrade && !allow_transport {
                log_warn!(
                    "live settings force_http downgrade ignored (was {:?} now {:?}): set RSC_LS_ALLOW_SETTINGS_TRANSPORT=1 to allow workspace transport overrides",
                    cfg.force_http,
                    http_val
                );
            } else {
                cfg.force_http = http_val;
            }
        }
        if let Some(timeout_val) = get_settings_u64(
            settings_obj,
            &["timeout", "timeout_secs", "MIKROTIK_TIMEOUT"],
        ) {
            cfg.timeout_secs = timeout_val.clamp(1, 30);
        }
        // Custom resources overlay: privileged, requires opt-in.
        let custom_present = settings_obj.get("custom_resources").is_some()
            || settings_obj.get("live_resources").is_some()
            || settings_obj.get("RSC_LS_LIVE_RESOURCES").is_some();
        if custom_present {
            if !allow_transport {
                log_warn!(
                    "live settings custom_resources ignored (was {} items): set RSC_LS_ALLOW_SETTINGS_TRANSPORT=1 to allow workspace transport overrides",
                    cfg.custom_resources.len()
                );
            } else if let Some(custom_val) = settings_obj
                .get("custom_resources")
                .or_else(|| settings_obj.get("live_resources"))
                .or_else(|| settings_obj.get("RSC_LS_LIVE_RESOURCES"))
            {
                if custom_val.is_array() {
                    cfg.custom_resources = parse_custom_resources_from_value(custom_val);
                } else if let Some(s) = custom_val.as_str() {
                    cfg.custom_resources = parse_custom_resources(Some(s));
                } else if let Some(s) = get_settings_str(settings_obj, &["RSC_LS_LIVE_RESOURCES"]) {
                    cfg.custom_resources = parse_custom_resources(Some(&s));
                }
            } else if let Some(s) = get_settings_str(settings_obj, &["RSC_LS_LIVE_RESOURCES"]) {
                cfg.custom_resources = parse_custom_resources(Some(&s));
            }
        }
        if let Some(allow) = get_settings_bool(
            settings_obj,
            &["allow_loopback", "RSC_LS_LIVE_ALLOW_LOOPBACK"],
        ) {
            let is_downgrade = allow && !cfg.allow_loopback;
            if is_downgrade && !allow_transport {
                log_warn!(
                    "live settings allow_loopback downgrade ignored (was {:?} now {:?}): set RSC_LS_ALLOW_SETTINGS_TRANSPORT=1 to allow workspace transport overrides",
                    cfg.allow_loopback,
                    allow
                );
            } else {
                cfg.allow_loopback = allow;
            }
        } else if let Some(s) = get_settings_str(settings_obj, &["RSC_LS_LIVE_ALLOW_LOOPBACK"]) {
            let requested = s.trim() == "1";
            let is_downgrade = requested && !cfg.allow_loopback;
            if is_downgrade && !allow_transport {
                log_warn!(
                    "live settings allow_loopback downgrade ignored (was {:?} now {:?}): set RSC_LS_ALLOW_SETTINGS_TRANSPORT=1 to allow workspace transport overrides",
                    cfg.allow_loopback,
                    requested
                );
            } else {
                cfg.allow_loopback = requested;
            }
        }
        // TLS pin overlay: adding a pin is hardening (always allowed);
        // removing a pin configured via env is a downgrade (needs opt-in).
        if let Some(fp_val) =
            get_settings_str(settings_obj, &["fingerprint", "MIKROTIK_FINGERPRINT"])
        {
            let trimmed = fp_val.trim();
            if trimmed.is_empty() {
                if cfg.fingerprint.is_some() && !allow_transport {
                    log_warn!(
                        "live settings fingerprint removal ignored (pin configured via env): set RSC_LS_ALLOW_SETTINGS_TRANSPORT=1 to allow workspace transport overrides"
                    );
                } else if cfg.fingerprint.is_some() {
                    cfg.fingerprint = None;
                }
            } else {
                let (parsed, invalid) = parse_fingerprint(Some(trimmed));
                if invalid {
                    log_warn!(
                        "live settings fingerprint invalid (expected sha256:<64 hex chars>), ignoring"
                    );
                } else {
                    cfg.fingerprint = parsed;
                    cfg.fingerprint_invalid = false;
                }
            }
        }
        // Custom CA overlay: workspace-provided trust anchors are privileged
        // (a malicious settings file could point at a rogue CA), so any
        // settings CA value requires the transport opt-in. Env always wins
        // when the opt-in is absent.
        if let Some(ca_val) = get_settings_str(settings_obj, &["ca_file", "MIKROTIK_CA_FILE"]) {
            let trimmed = ca_val.trim().to_string();
            if trimmed != cfg.ca_file {
                if !allow_transport {
                    log_warn!(
                        "live settings ca_file ignored (was {:?} now {:?}): set RSC_LS_ALLOW_SETTINGS_TRANSPORT=1 to allow workspace transport overrides",
                        sanitize_for_log(&cfg.ca_file),
                        sanitize_for_log(&trimmed)
                    );
                } else {
                    cfg.ca_file = trimmed;
                }
            }
        }
        // Log if multi-host after overlay
        if cfg.hosts.len() > 1 {
            log_info!(
                "live multi-host (settings) {} (primary={})",
                cfg.hosts.len(),
                sanitize_for_log(&cfg.host)
            );
        }
    }

    /// Resolve a menu/property/type to a live resource, checking custom resources
    /// as fallback when the hardcoded heuristic returns `None`.
    ///
    /// Keeps hardcoded heuristic for backward compat; custom resources are
    /// matched by property name (case-insensitive).
    pub fn resolve_resource_with_custom(
        &self,
        menu_path: &str,
        property: &str,
        type_str: &str,
    ) -> Option<ResourceKind> {
        if let Some(kind) = live_resource_for_menu_property(menu_path, property, type_str) {
            return Some(kind);
        }
        // Fallback to custom resources: if property matches a custom mapping, treat as interface-like.
        // We map custom to the closest built-in kind for now, or return Interfaces as generic.
        let prop_low = property.to_ascii_lowercase();
        for cr in &self.custom_resources {
            if cr.property.eq_ignore_ascii_case(&prop_low)
                || cr.property.eq_ignore_ascii_case(property)
            {
                // Custom resource matched — we still need a ResourceKind to drive cache key.
                // For now, return Interfaces as a generic live kind; future: use custom path/field directly.
                // Better: return a dedicated handling via custom fetch; but for completion we can treat as live.
                // We log and return Interfaces to keep cache isolation simple.
                log_debug!(
                    "live custom resource matched property={} path={} field={}",
                    sanitize_for_log(&cr.property),
                    sanitize_for_log(&cr.path),
                    sanitize_for_log(&cr.field)
                );
                return Some(ResourceKind::Interfaces);
            }
        }
        None
    }

    /// Get custom resource descriptor for a property if present (case-insensitive).
    pub fn custom_resource_for_property(&self, property: &str) -> Option<&CustomResource> {
        let prop_low = property.to_ascii_lowercase();
        self.custom_resources
            .iter()
            .find(|cr| cr.property.eq_ignore_ascii_case(&prop_low))
    }
}

/// Parse `MIKROTIK_HOST` comma-separated list into validated hosts, capped to `LIVE_MAX_HOSTS`.
fn parse_hosts(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut hosts: Vec<String> = trimmed
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if hosts.len() > LIVE_MAX_HOSTS {
        log_warn!(
            "live hosts count {} exceeds cap {}, truncating",
            hosts.len(),
            LIVE_MAX_HOSTS
        );
        hosts.truncate(LIVE_MAX_HOSTS);
    }
    // Validate each host; keep only valid ones for the vec but keep primary as first valid?
    // For now keep all but log warnings for invalid ones. is_active checks primary.
    for h in &hosts {
        if let Err(e) = validate_host(h) {
            log_warn!(
                "live host validation failed for {:?}: {e}",
                sanitize_for_log(h)
            );
        }
        if is_ssrf_denied_host(h) {
            log_warn!("live host denied by SSRF filter: {:?}", sanitize_for_log(h));
        }
    }
    hosts
}

/// Parse custom resources from an optional JSON string (env var).
fn parse_custom_resources(raw: Option<&str>) -> Vec<CustomResource> {
    let Some(s) = raw else {
        return Vec::new();
    };
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    // Try to parse as JSON array.
    let v: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => {
            log_warn!("invalid RSC_LS_LIVE_RESOURCES JSON, ignoring: {e}");
            return Vec::new();
        }
    };
    parse_custom_resources_from_value(&v)
}

/// Validate a custom resource field or property name.
///
/// Must match `^[a-zA-Z0-9_-]+$` and be 1..64 chars, mirroring the filename
/// allowlist style. Rejects null, control, and URI delimiters implicitly via
/// the regex.
fn is_valid_custom_identifier(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_LIVE_VALUE_LEN {
        return false;
    }
    if s.contains('\0') || s.chars().any(|c| c.is_control()) {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Validate a custom resource REST path.
///
/// Requirements:
/// - exactly `/rest` or starts with `/rest/`, length 1..64
/// - no null, control, `\`, `%`, `?`, `#`, `@`
/// - no `//` (consecutive slashes)
/// - no `..` as an exact segment (split by `/`)
pub(crate) fn is_valid_custom_path(path: &str) -> bool {
    if path.is_empty() || path.len() > 64 {
        return false;
    }
    // F7: bare `/rest`, `/restful`, `/restx` must not pass. Only the exact
    // root or the `/rest/` prefix is a device REST path.
    if path != "/rest" && !path.starts_with("/rest/") {
        return false;
    }
    if path.contains('\0') || path.chars().any(|c| c.is_control()) {
        return false;
    }
    if path.contains('\\')
        || path.contains('%')
        || path.contains('?')
        || path.contains('#')
        || path.contains('@')
    {
        return false;
    }
    if path.contains("//") {
        return false;
    }
    if path.split('/').any(|seg| seg == "..") {
        return false;
    }
    true
}

/// Parse custom resources from a `serde_json::Value` (settings overlay).
fn parse_custom_resources_from_value(v: &serde_json::Value) -> Vec<CustomResource> {
    let arr = match v.as_array() {
        Some(a) => a,
        None => {
            log_warn!("RSC_LS_LIVE_RESOURCES expected JSON array, got {:?}", v);
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for entry in arr.iter().take(LIVE_CUSTOM_RESOURCES_MAX) {
        let Some(obj) = entry.as_object() else {
            continue;
        };
        let property = obj
            .get("property")
            .and_then(|p| p.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let path = obj
            .get("path")
            .and_then(|p| p.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let field = obj
            .get("field")
            .and_then(|p| p.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if property.is_empty() || path.is_empty() || field.is_empty() {
            log_warn!(
                "custom resource missing required fields, skipping: {:?}",
                sanitize_for_log(&format!("{entry:?}"))
            );
            continue;
        }
        // Path validation: strict allowlist, no traversal or delimiters.
        if !is_valid_custom_path(&path) {
            log_warn!(
                "custom resource path failed validation, skipping: {:?}",
                sanitize_for_log(&format!("{entry:?}"))
            );
            continue;
        }
        // Property and field validation: ^[a-zA-Z0-9_-]+$ 1..64
        if !is_valid_custom_identifier(&property) {
            log_warn!(
                "custom resource property failed validation (expected ^[a-zA-Z0-9_-]+$ 1..64), skipping: {:?}",
                sanitize_for_log(&format!("{entry:?}"))
            );
            continue;
        }
        if !is_valid_custom_identifier(&field) {
            log_warn!(
                "custom resource field failed validation (expected ^[a-zA-Z0-9_-]+$ 1..64), skipping: {:?}",
                sanitize_for_log(&format!("{entry:?}"))
            );
            continue;
        }
        out.push(CustomResource {
            property,
            path,
            field,
        });
    }
    if arr.len() > LIVE_CUSTOM_RESOURCES_MAX {
        log_warn!(
            "custom resources count {} exceeds cap {}, truncating",
            arr.len(),
            LIVE_CUSTOM_RESOURCES_MAX
        );
    }
    out
}

/// Find the most relevant settings object inside a `didChangeConfiguration` value.
///
/// Only explicit scopes resolve: `rsc.live`, `rsc` (itself or carrying host
/// keys), `mikrotik`, or `settings.*` nesting thereof. A bare object is
/// NEVER treated as a settings object, even when it contains host-like
/// keys — otherwise any unrelated editor payload with a `host` field could
/// hijack the device connection. Returns `None` when no scope matches.
fn find_settings_object(v: &serde_json::Value) -> Option<&serde_json::Value> {
    if let Some(obj) = v.as_object() {
        // Direct rsc.live
        if let Some(rsc) = obj.get("rsc") {
            if let Some(live) = rsc.get("live") {
                return Some(live);
            }
            // rsc itself might contain host etc.
            if rsc.get("host").is_some() || rsc.get("MIKROTIK_HOST").is_some() {
                return Some(rsc);
            }
        }
        if let Some(mikrotik) = obj.get("mikrotik") {
            return Some(mikrotik);
        }
        if let Some(settings) = obj.get("settings") {
            return find_settings_object(settings);
        }
    }
    None
}

fn get_settings_str(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(val) = v.get(*k) {
            if let Some(s) = val.as_str() {
                return Some(s.to_string());
            }
            if let Some(n) = val.as_u64() {
                return Some(n.to_string());
            }
            if let Some(b) = val.as_bool() {
                return Some(if b { "1".to_string() } else { "0".to_string() });
            }
        }
    }
    None
}

fn get_settings_bool(v: &serde_json::Value, keys: &[&str]) -> Option<bool> {
    for k in keys {
        if let Some(val) = v.get(*k) {
            if let Some(b) = val.as_bool() {
                return Some(b);
            }
            if let Some(s) = val.as_str() {
                let t = s.trim().to_ascii_lowercase();
                if t == "1" || t == "true" || t == "yes" {
                    return Some(true);
                }
                if t == "0" || t == "false" || t == "no" {
                    return Some(false);
                }
            }
            if let Some(n) = val.as_u64() {
                return Some(n != 0);
            }
        }
    }
    None
}

fn get_settings_u64(v: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    for k in keys {
        if let Some(val) = v.get(*k) {
            if let Some(n) = val.as_u64() {
                return Some(n);
            }
            if let Some(s) = val.as_str()
                && let Ok(n) = s.trim().parse::<u64>()
            {
                return Some(n);
            }
        }
    }
    None
}

fn get_settings_port(v: &serde_json::Value) -> Option<u16> {
    for k in &["port", "MIKROTIK_PORT"] {
        if let Some(val) = v.get(*k) {
            if let Some(n) = val.as_u64()
                && (1..=65535).contains(&n)
            {
                return Some(n as u16);
            }
            if let Some(s) = val.as_str()
                && let Ok(n) = s.trim().parse::<i64>()
                && (1..=65535).contains(&n)
            {
                return Some(n as u16);
            }
        }
    }
    None
}

/// Parse an env integer with deploy-companion warning semantics.
///
/// `raw`: the `Option<String>` from env (None => default).
/// Returns `default` on empty, missing, or parse failure (with warning).
fn parse_env_u16(raw: &Option<String>, default: u16, min: u16, max: u16, name: &str) -> u16 {
    let Some(s) = raw else {
        return default;
    };
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return default;
    }
    match trimmed.parse::<i64>() {
        Ok(v) => {
            if v < min as i64 || v > max as i64 {
                log_warn!(
                    "invalid {}={:?}, expected {}..{}, using default {}",
                    name,
                    s,
                    min,
                    max,
                    default
                );
                default
            } else {
                v as u16
            }
        }
        Err(_) => {
            log_warn!("invalid {}={:?}, using default {}", name, s, default);
            default
        }
    }
}

fn parse_env_u64(raw: &Option<String>, default: u64, name: &str) -> u64 {
    let Some(s) = raw else {
        return default;
    };
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return default;
    }
    match trimmed.parse::<i64>() {
        Ok(v) if v < 0 => {
            log_warn!("invalid {}={:?}, using default {}", name, s, default);
            default
        }
        Ok(v) => v as u64,
        Err(_) => {
            log_warn!("invalid {}={:?}, using default {}", name, s, default);
            default
        }
    }
}

/// Whether the legacy `--no-ssl-verify` silent HTTP downgrade is allowed.
///
/// Default OFF (matches `scripts/_mikrotik_shared.py`, which removed the
/// shim): `MIKROTIK_SSL=0` only disables verification, never the scheme.
/// Opt-in via `RSC_LS_LEGACY_HTTP_SHIM=1` (or `MIKROTIK_LEGACY_HTTP_SHIM=1`)
/// restores the historical fallback (non-standard port + verify off => http)
/// with a WARN. Plain HTTP otherwise requires explicit `MIKROTIK_HTTP=1`.
fn legacy_http_shim_allowed() -> bool {
    std::env::var("RSC_LS_LEGACY_HTTP_SHIM")
        .ok()
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
        || std::env::var("MIKROTIK_LEGACY_HTTP_SHIM")
            .ok()
            .map(|v| v.trim() == "1")
            .unwrap_or(false)
}

/// Test-friendly legacy check against an injected env getter.
#[cfg(test)]
pub(crate) fn legacy_http_shim_allowed_with<F>(get: &F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    get("RSC_LS_LEGACY_HTTP_SHIM")
        .as_deref()
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
        || get("MIKROTIK_LEGACY_HTTP_SHIM")
            .as_deref()
            .map(|v| v.trim() == "1")
            .unwrap_or(false)
}

/// Resolve the REST URL scheme, mirroring `scripts/_mikrotik_shared.py::resolve_scheme`.
///
/// `port`: target port.
/// `force_http`: `MIKROTIK_HTTP=1`.
/// `ssl_verify`: true when verification is enabled; false when `MIKROTIK_SSL=0`.
///
/// Default is HTTPS on every port; `MIKROTIK_SSL=0` only controls
/// certificate validation, never the scheme. The historical silent
/// downgrade (non-standard port + verify off => http) fires only when the
/// legacy opt-in (`RSC_LS_LEGACY_HTTP_SHIM=1`) is set, and then with a WARN.
/// Without the opt-in the same combination stays on HTTPS with a WARN
/// telling the operator to pass `MIKROTIK_HTTP=1` for plain HTTP.
pub(crate) fn resolve_scheme(port: u16, force_http: bool, ssl_verify: bool) -> &'static str {
    let (scheme, _fired) =
        resolve_scheme_with_legacy(port, force_http, ssl_verify, legacy_http_shim_allowed());
    scheme
}

/// Test-friendly scheme resolution with an explicit legacy flag.
///
/// Returns `(scheme, legacy_shim_fired)`: the flag is true when the legacy
/// condition (non-standard port + verify off, without force_http) matched,
/// regardless of whether the opt-in allowed the downgrade.
pub(crate) fn resolve_scheme_with_legacy(
    port: u16,
    force_http: bool,
    ssl_verify: bool,
    allow_legacy_shim: bool,
) -> (&'static str, bool) {
    let no_ssl_verify = !ssl_verify;
    let legacy_condition = !force_http && no_ssl_verify && port != 443 && port != 8729;
    if legacy_condition {
        if allow_legacy_shim {
            log_warn!(
                "legacy http shim: MIKROTIK_SSL=0 on non-standard port {port} downgraded scheme to http (opt-in RSC_LS_LEGACY_HTTP_SHIM=1); prefer MIKROTIK_HTTP=1 for plain HTTP"
            );
            return ("http", true);
        }
        log_warn!(
            "MIKROTIK_SSL=0 does not select the scheme; staying on https for port {port} (pass MIKROTIK_HTTP=1 for plain HTTP)"
        );
        return ("https", true);
    }
    if force_http {
        ("http", false)
    } else {
        ("https", false)
    }
}

/// Parse `MIKROTIK_FINGERPRINT=sha256:<hex>` into 32 raw bytes.
///
/// Accepts `sha256:` prefix case-insensitively, strips embedded `:`/space
/// separators, and requires exactly 64 hex chars (32 bytes, SPKI SHA256).
/// Returns `(Some(bytes), false)` on success, `(None, false)` when unset or
/// empty, and `(None, true)` when present but malformed (fail-closed: the
/// caller must refuse to go active). Malformed values log a WARN without
/// echoing the value.
pub(crate) fn parse_fingerprint(raw: Option<&str>) -> (Option<[u8; 32]>, bool) {
    let Some(v) = raw else {
        return (None, false);
    };
    let trimmed = v.trim();
    if trimmed.is_empty() {
        return (None, false);
    }
    let mut hex = trimmed;
    if hex.len() >= 7 && hex[..7].eq_ignore_ascii_case("sha256:") {
        hex = &hex[7..];
    }
    let compact: String = hex
        .chars()
        .filter(|c| *c != ':' && !c.is_whitespace())
        .collect();
    if compact.len() != 64 || !compact.chars().all(|c| c.is_ascii_hexdigit()) {
        log_warn!(
            "invalid MIKROTIK_FINGERPRINT (expected sha256:<64 hex chars>), ignoring fail-closed"
        );
        return (None, true);
    }
    let mut out = [0u8; 32];
    for (i, chunk) in compact.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16).unwrap_or(0);
        let lo = (chunk[1] as char).to_digit(16).unwrap_or(0);
        out[i] = ((hi << 4) | lo) as u8;
    }
    (Some(out), false)
}

// ── Minimal SHA256 + SPKI extraction (no new deps) ─────────────────
//
// The pin compares `SHA256(DER(subjectPublicKeyInfo))` of the leaf cert
// (RFC 7469 style). `ring` is not a direct dependency, so a compact pure-Rust
// SHA256 is vendored here (~64 rounds, standard constants). SPKI bytes are
// located with a minimal DER TLV walker (fail-closed `None` on malformed).

pub(crate) fn sha256_block(state: &mut [u32; 8], block: &[u8; 64]) {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut w = [0u32; 64];
    for (i, c) in block.chunks(4).enumerate().take(16) {
        w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h) = (
        state[0], state[1], state[2], state[3], state[4], state[5], state[6], state[7],
    );
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

/// SHA256 over `data` (pure Rust, no extra dependency).
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    for block in padded.chunks(64) {
        let mut arr = [0u8; 64];
        arr.copy_from_slice(block);
        sha256_block(&mut state, &arr);
    }
    let mut out = [0u8; 32];
    for (i, v) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// Read one DER TLV at `data[pos..]`; returns `(tag, header_len, value_len, value_start)`.
fn der_tlv(data: &[u8], pos: usize) -> Option<(u8, usize, usize, usize)> {
    if pos + 2 > data.len() {
        return None;
    }
    let tag = data[pos];
    let first = data[pos + 1];
    if first & 0x80 == 0 {
        let len = (first & 0x7f) as usize;
        let start = pos + 2;
        if start + len > data.len() {
            return None;
        }
        return Some((tag, 2, len, start));
    }
    let nbytes = (first & 0x7f) as usize;
    if nbytes == 0 || nbytes > 4 || pos + 2 + nbytes > data.len() {
        return None;
    }
    let mut len: usize = 0;
    for b in &data[pos + 2..pos + 2 + nbytes] {
        len = len.checked_mul(256)?.checked_add(*b as usize)?;
    }
    let start = pos + 2 + nbytes;
    if start + len > data.len() {
        return None;
    }
    Some((tag, 2 + nbytes, len, start))
}

/// Extract the full DER TLV of `subjectPublicKeyInfo` from a DER certificate.
///
/// Walks `Certificate ::= SEQUENCE { tbsCertificate SEQUENCE { ... } }` and
/// returns the SPKI SEQUENCE TLV (tag + length + value). Fail-closed `None`
/// on any malformed input.
pub(crate) fn extract_spki_der(cert_der: &[u8]) -> Option<&[u8]> {
    // Outer Certificate SEQUENCE.
    let (tag, hlen, _vlen, vstart) = der_tlv(cert_der, 0)?;
    if tag != 0x30 {
        return None;
    }
    // First child of Certificate is tbsCertificate SEQUENCE.
    let (tbs_tag, _tbs_hlen, tbs_len, tbs_start) = der_tlv(cert_der, vstart)?;
    if tbs_tag != 0x30 {
        return None;
    }
    let _ = hlen;
    let tbs_end = tbs_start + tbs_len;
    if tbs_end > cert_der.len() {
        return None;
    }
    let mut pos = tbs_start;
    // Optional [0] version (context-specific constructed 0xA0).
    if pos < tbs_end && cert_der[pos] == 0xA0 {
        let (_, h, l, s) = der_tlv(cert_der, pos)?;
        pos = s + l;
        let _ = h;
    }
    // serial INTEGER, signature SEQUENCE, issuer Name, validity SEQUENCE,
    // subject Name — skip each generically, then SPKI is next.
    for _ in 0..5 {
        if pos >= tbs_end {
            return None;
        }
        let (_, h, l, s) = der_tlv(cert_der, pos)?;
        pos = s + l;
        let _ = h;
    }
    if pos >= tbs_end || cert_der[pos] != 0x30 {
        return None;
    }
    let (_, h, l, s) = der_tlv(cert_der, pos)?;
    let total = h + l;
    if pos + total > cert_der.len() {
        return None;
    }
    let _ = s;
    Some(&cert_der[pos..pos + total])
}

/// SHA256 of the leaf certificate SPKI (fail-closed `None` on malformed DER).
pub(crate) fn spki_sha256(cert_der: &[u8]) -> Option<[u8; 32]> {
    let spki = extract_spki_der(cert_der)?;
    Some(sha256(spki))
}

/// Check if a host is denied by SSRF protection.
pub(crate) fn is_ssrf_denied_host(host: &str) -> bool {
    // Normalize: lowercase, strip brackets, strip port if present? host here is without port.
    let lower = host.trim().to_ascii_lowercase();
    // Strip IPv6 brackets for comparison
    let inner = if lower.starts_with('[') && lower.ends_with(']') {
        &lower[1..lower.len() - 1]
    } else {
        &lower
    };
    // Exact denials
    if inner == "169.254.169.254" {
        return true;
    }
    if inner == "metadata.google.internal" {
        return true;
    }
    // Minimal alias denials (exact hosts only, no DNS resolution): bare
    // `metadata.google` and `metadata.goog` are well-known metadata
    // endpoints. No suffix matching to avoid over-blocking normal hosts.
    if inner == "metadata.google" {
        return true;
    }
    if inner == "metadata.goog" {
        return true;
    }
    // Note: the zone-id form "::ffff:169.254.169.254%lo0" is intentionally not
    // listed here because validate_host rejects '%' in hosts, making such a
    // literal unreachable. If SSRF checks were moved before validation, this
    // branch would need reconsideration, but after validation it is dead code.
    if inner == "::ffff:169.254.169.254" {
        return true;
    }
    // Also deny the IPv6 bracketed form already handled via inner, but check original with brackets
    if lower == "[169.254.169.254]" {
        return true;
    }
    // Unspecified addresses are never valid RouterOS hosts — deny them as SSRF.
    if inner == "0.0.0.0" || inner == "::" {
        return true;
    }
    if lower == "[0.0.0.0]" || lower == "[::]" {
        return true;
    }
    false
}

/// Normalize `host` via WHATWG URL parsing and return the IP when numeric.
///
/// The HTTP client connects to the WHATWG-normalized host, so all range
/// checks must run against this value — not the raw string. Lexical checks
/// alone miss decimal (`2130706433`), hex (`0x7f000001`), short
/// (`127.1`), octal (`0177.0.0.1`), and mapped (`::ffff:127.0.0.1`) forms.
/// Returns `None` for domain names or unparsable hosts (callers fall back
/// to lexical hostname checks).
pub(crate) fn normalized_host_ip(host: &str) -> Option<std::net::IpAddr> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return None;
    }
    let host_for_url = format_host_for_url(trimmed);
    let url_str = format!("http://{host_for_url}/");
    let parsed = url::Url::parse(&url_str).ok()?;
    match parsed.host()? {
        url::Host::Ipv4(v4) => Some(std::net::IpAddr::V4(v4)),
        url::Host::Ipv6(v6) => Some(std::net::IpAddr::V6(v6)),
        url::Host::Domain(_) => None,
    }
}

/// Whether `host` is a non-canonical numeric literal.
///
/// When WHATWG normalization yields an IP whose canonical string differs
/// from the raw literal (modulo brackets and ASCII case), the input used a
/// decimal/hex/octal/short or otherwise non-canonical encoding and is
/// rejected fail-closed — even when the normalized address itself would be
/// public. Canonical forms (`127.0.0.1`, `8.8.8.8`, `2001:db8::1`) are
/// unaffected because they already match their canonical string.
pub(crate) fn is_non_canonical_numeric_host(host: &str) -> bool {
    let trimmed = host.trim();
    let inner = if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2 {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    let Some(normalized) = normalized_host_ip(trimmed) else {
        return false;
    };
    let canonical = normalized.to_string().to_ascii_lowercase();
    inner.to_ascii_lowercase() != canonical
}

/// Whether a normalized IP is unconditionally SSRF-denied.
///
/// Covers whole `169.254.0.0/16` link-local (not just `.169.254`),
/// IPv6 `fe80::/10` link-local, and unspecified addresses. IPv4-mapped
/// IPv6 (`::ffff:a.b.c.d`) is mapped to IPv4 before the check so
/// `[::ffff:a9fe:a9fe]` (metadata IP) is denied as link-local.
pub(crate) fn is_normalized_ssrf_denied(addr: std::net::IpAddr) -> bool {
    match addr {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            if o[0] == 169 && o[1] == 254 {
                return true;
            }
            if v4.is_unspecified() {
                return true;
            }
            false
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_normalized_ssrf_denied(std::net::IpAddr::V4(mapped));
            }
            if v6.is_unspecified() {
                return true;
            }
            // fe80::/10: first 10 bits are 1111111010.
            if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                return true;
            }
            false
        }
    }
}

/// Whether a normalized IP is loopback or RFC1918 private.
///
/// IPv4-mapped IPv6 is mapped to IPv4 first so `[::ffff:127.0.0.1]` and
/// `[::ffff:10.0.0.1]` are judged as their IPv4 equivalents. ULA
/// (`fc00::/7`) stays allowed to avoid over-blocking, matching prior policy.
pub(crate) fn is_normalized_loopback_or_private(addr: std::net::IpAddr) -> bool {
    match addr {
        std::net::IpAddr::V4(v4) => {
            if v4.is_loopback() {
                return true;
            }
            let o = v4.octets();
            if o[0] == 10 {
                return true;
            }
            if o[0] == 192 && o[1] == 168 {
                return true;
            }
            if o[0] == 172 && (16..=31).contains(&o[1]) {
                return true;
            }
            false
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_normalized_loopback_or_private(std::net::IpAddr::V4(mapped));
            }
            if v6.is_loopback() {
                return true;
            }
            false
        }
    }
}

/// Whether `host` is loopback or private (RFC1918 / ULA / loopback).
///
/// Used for `RSC_LS_LIVE_ALLOW_LOOPBACK` gating: when loopback is not
/// allowed, these hosts are SSRF-denied. Handles IPv4 and IPv6 literals,
/// bracketed IPv6 (`[::1]`), and the hostname `localhost`. Range checks run
/// against the WHATWG-normalized IP (via `normalized_host_ip` with IPv4
///-mapped unmapping) so decimal/hex/short/mapped bypasses are closed.
pub(crate) fn is_loopback_or_private(host: &str) -> bool {
    // Normalize-then-check: the HTTP client connects to the normalized host.
    if let Some(normalized) = normalized_host_ip(host)
        && is_normalized_loopback_or_private(normalized)
    {
        return true;
    }
    let lower = host.trim().to_ascii_lowercase();
    let inner = if lower.starts_with('[') && lower.ends_with(']') {
        &lower[1..lower.len() - 1]
    } else {
        &lower
    };
    if inner == "localhost" {
        return true;
    }
    // Strip zone id / scope (e.g. fe80::1%lo0) for parsing.
    let host_no_zone = inner.split('%').next().unwrap_or(inner);
    if let Ok(addr) = host_no_zone.parse::<std::net::IpAddr>() {
        if addr.is_loopback() {
            return true;
        }
        // RFC1918 private for IPv4; ULA (fc00::/7) is considered private but not required for this flag.
        match addr {
            std::net::IpAddr::V4(v4) => {
                let o = v4.octets();
                if o[0] == 10 {
                    return true;
                }
                if o[0] == 192 && o[1] == 168 {
                    return true;
                }
                if o[0] == 172 && (16..=31).contains(&o[1]) {
                    return true;
                }
            }
            std::net::IpAddr::V6(_) => {
                // Loopback already handled; ULA not denied by default to avoid over-blocking.
            }
        }
    }
    false
}

/// Whether loopback/private hosts are allowed via `RSC_LS_LIVE_ALLOW_LOOPBACK=1`.
fn live_allow_loopback() -> bool {
    std::env::var("RSC_LS_LIVE_ALLOW_LOOPBACK")
        .ok()
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// Validate `host` with explicit loopback allowance.
pub fn validate_host_with_allow(host: &str, allow_loopback: bool) -> Result<(), LiveError> {
    if host.is_empty() {
        return Err(LiveError::InvalidHost("empty".to_string()));
    }
    if host.len() > 253 {
        return Err(LiveError::InvalidHost("exceeds 253 chars".to_string()));
    }
    if host.contains('\0') {
        return Err(LiveError::InvalidHost("contains null byte".to_string()));
    }
    if host.chars().any(|c| c.is_control()) {
        return Err(LiveError::InvalidHost(
            "contains control characters".to_string(),
        ));
    }
    if host.contains('@')
        || host.contains('?')
        || host.contains('#')
        || host.contains(' ')
        || host.contains('%')
    {
        return Err(LiveError::InvalidHost("contains URI delimiter".to_string()));
    }
    if is_ssrf_denied_host(host) {
        return Err(LiveError::InvalidHost("SSRF denied host".to_string()));
    }
    // Normalize-then-check: run ALL range checks against the WHATWG-normalized
    // host the HTTP client actually connects to. Rejects non-canonical numeric
    // literals fail-closed, then whole 169.254.0.0/16 and fe80::/10, then
    // loopback/RFC1918 against the normalized IP with IPv4-mapped unmapping.
    if is_non_canonical_numeric_host(host) {
        return Err(LiveError::InvalidHost(
            "non-canonical numeric host".to_string(),
        ));
    }
    if let Some(normalized) = normalized_host_ip(host) {
        if is_normalized_ssrf_denied(normalized) {
            return Err(LiveError::InvalidHost("SSRF denied host".to_string()));
        }
        if !allow_loopback && is_normalized_loopback_or_private(normalized) {
            log_warn!(
                "live host denied (loopback/private) without RSC_LS_LIVE_ALLOW_LOOPBACK=1: {:?}",
                sanitize_for_log(host)
            );
            return Err(LiveError::InvalidHost(
                "loopback/private denied without RSC_LS_LIVE_ALLOW_LOOPBACK=1".to_string(),
            ));
        }
    }
    if !allow_loopback && is_loopback_or_private(host) {
        log_warn!(
            "live host denied (loopback/private) without RSC_LS_LIVE_ALLOW_LOOPBACK=1: {:?}",
            sanitize_for_log(host)
        );
        return Err(LiveError::InvalidHost(
            "loopback/private denied without RSC_LS_LIVE_ALLOW_LOOPBACK=1".to_string(),
        ));
    }
    Ok(())
}

/// Validate `host` per defensive rules.
///
/// - non-empty, max 253 chars
/// - no null bytes, no control chars
/// - no URI delimiters that would alter URL parsing (`@`, `?`, `#`, ` `, `%`)
/// - SSRF denials for whole `169.254.0.0/16`, IPv6 `fe80::/10`, unspecified,
///   and `metadata.google.internal` (lexical plus WHATWG-normalized checks)
/// - non-canonical numeric literals rejected fail-closed
/// - loopback/private denied unless `RSC_LS_LIVE_ALLOW_LOOPBACK=1` (via
///   `is_loopback_or_private` against the normalized IP with IPv4-mapped unmapping)
pub fn validate_host(host: &str) -> Result<(), LiveError> {
    validate_host_with_allow(host, live_allow_loopback())
}

/// F4: TLS-identity portion of the connection-invalidation predicate.
///
/// Returns true when the pin / pin-validity / CA-bundle selection changed.
/// The caller (`server::live_connection_changed`) ORs this with the
/// transport fields so entries fetched under a previous trust anchor are
/// never reused after a pin/CA rotation.
pub(crate) fn live_identity_changed(old: &LiveConfig, new: &LiveConfig) -> bool {
    old.fingerprint != new.fingerprint
        || old.fingerprint_invalid != new.fingerprint_invalid
        || old.ca_file != new.ca_file
}

// ── F1: resolve-then-revalidate (DNS TOCTOU) ─────────────────────
//
// Lexical + normalized-literal checks run at config time, but a hostname
// can resolve to a denied address at fetch time (DNS rebinding / split
// horizon). Re-resolve at fetch time and re-run the deny policy against
// EVERY returned IP, fail-closed: any denied IP, or any resolution
// failure, refuses the fetch before credentials are sent.

/// Classify one resolved IP against the SSRF deny policy.
///
/// Returns `Some(reason)` when the address is denied, `None` when allowed.
/// Unconditionally denied: whole `169.254.0.0/16`, IPv6 `fe80::/10`,
/// unspecified, and — unless `allow_loopback` — loopback/RFC1918.
pub(crate) fn denied_reason_for_ip(
    addr: std::net::IpAddr,
    allow_loopback: bool,
) -> Option<&'static str> {
    if is_normalized_ssrf_denied(addr) {
        return Some("SSRF denied resolved IP");
    }
    if !allow_loopback && is_normalized_loopback_or_private(addr) {
        return Some("loopback/private denied without RSC_LS_LIVE_ALLOW_LOOPBACK=1");
    }
    None
}

/// Resolve `host:port` and deny the fetch when ANY resolved IP is denied.
///
/// - IP literals resolve locally (no DNS) via `ToSocketAddrs`.
/// - Hostnames resolve via the system resolver (`getaddrinfo`).
/// - Empty results or resolution failures are fail-closed (`InvalidHost`).
/// - Every returned IP is checked with [`denied_reason_for_ip`]; the first
///   denial fails the whole fetch (an attacker controls only one record to
///   win a race).
pub(crate) fn resolve_and_validate_host(
    host: &str,
    port: u16,
    allow_loopback: bool,
) -> Result<(), LiveError> {
    use std::net::ToSocketAddrs;
    let bare = host.trim().trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<std::net::IpAddr> = (bare, port)
        .to_socket_addrs()
        .map(|iter| iter.map(|s| s.ip()).collect())
        .map_err(|e| LiveError::InvalidHost(format!("dns resolution failed: {e}")))?;
    if addrs.is_empty() {
        return Err(LiveError::InvalidHost(
            "dns resolution returned no addresses".to_string(),
        ));
    }
    for addr in addrs {
        if let Some(reason) = denied_reason_for_ip(addr, allow_loopback) {
            log_warn!(
                "live fetch denied: host {:?} resolved to denied IP {addr} ({reason})",
                sanitize_for_log(host)
            );
            return Err(LiveError::InvalidHost(format!(
                "resolved IP denied: {addr}"
            )));
        }
    }
    Ok(())
}

// ── F8: bounded CA-bundle loading ────────────────────────────────

/// Max bytes read from `MIKROTIK_CA_FILE` (256 KiB — PEM bundles are small;
/// anything larger is a misconfiguration, not a trust anchor).
pub(crate) const MAX_CA_FILE_BYTES: u64 = 256 * 1024;

/// Negative cache of CA paths that failed to parse, so every completion
/// keystroke does not re-read a broken bundle. Keyed by the canonical path
/// when available, else the raw path. Entries are never evicted within the
/// process lifetime (a bundle fix requires a restart — documented in the
/// WARN at insertion).
fn bad_ca_cache() -> &'static Mutex<HashSet<String>> {
    static BAD_CA: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    BAD_CA.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(crate) fn is_bad_ca(key: &str) -> bool {
    bad_ca_cache()
        .lock()
        .map(|g| g.contains(key))
        .unwrap_or(false)
}

fn mark_bad_ca(key: String) {
    if let Ok(mut g) = bad_ca_cache().lock() {
        g.insert(key);
    }
}

/// Read a CA bundle with a 256 KiB cap, symlink warning, and negative cache.
///
/// - Resolves `canonicalize()` for the cache key; warns when the canonical
///   path differs (symlink or `..`/curdir indirection) so a swapped link
///   target is visible in logs.
/// - Warns (once per path) when the file is a symlink.
/// - Returns `None` fail-closed on missing/unreadable/oversize/undecodable
///   input; callers fall back to the default verifier (never insecure).
pub(crate) fn read_ca_bundle(ca_file: &str) -> Option<String> {
    if ca_file.trim().is_empty() {
        return None;
    }
    let raw_path = std::path::Path::new(ca_file);
    let canonical = std::fs::canonicalize(raw_path).ok();
    let key = canonical
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| ca_file.to_string());
    if is_bad_ca(&key) {
        return None;
    }
    let fail = |why: &str| {
        log_warn!(
            "live CA file unreadable ({why}): {:?}",
            sanitize_for_log(ca_file)
        );
        mark_bad_ca(key.clone());
        None
    };
    let meta = std::fs::symlink_metadata(raw_path).ok()?;
    if meta.file_type().is_symlink() {
        log_warn!(
            "live CA file is a symlink (target swap risk): {:?} -> {:?}",
            sanitize_for_log(ca_file),
            sanitize_for_log(
                &canonical
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default()
            )
        );
    }
    if canonical.as_ref().is_some_and(|c| {
        c.to_string_lossy() != raw_path.to_string_lossy().replace('\\', "/").as_str()
            && c.to_string_lossy() != ca_file
    }) {
        log_debug!(
            "live CA file canonicalized {:?} -> {:?}",
            sanitize_for_log(ca_file),
            sanitize_for_log(&key)
        );
    }
    if !meta.is_file() && !meta.file_type().is_symlink() {
        return fail("not a regular file");
    }
    if meta.len() > MAX_CA_FILE_BYTES {
        return fail("exceeds 256KiB cap");
    }
    let bytes = std::fs::read(raw_path).ok()?;
    if bytes.len() as u64 > MAX_CA_FILE_BYTES {
        return fail("exceeds 256KiB cap");
    }
    let text = String::from_utf8(bytes).ok()?;
    if parse_pem_certs(&text).is_empty() {
        return fail("no decodable PEM certificates");
    }
    Some(text)
}

/// Format host for URL: wrap bare IPv6 literals with brackets if needed.
///
/// Keep in sync with `scripts/_mikrotik_shared.py::format_host_for_url`.
pub(crate) fn format_host_for_url(host: &str) -> String {
    // Already bracketed? keep as is.
    if host.starts_with('[') && host.ends_with(']') {
        return host.to_string();
    }
    // Contains colon => likely IPv6 literal without brackets -> wrap.
    if host.contains(':') {
        return format!("[{host}]");
    }
    host.to_string()
}

/// Shared base URL builder — single source for host/port/scheme validation.
///
/// Validates host (`validate_host_with_allow`, SSRF, slash, port), wraps bare
/// IPv6, parses via `url::Url::parse`, and checks scheme. Path is left as `/`
/// for callers to set via `Url::set_path`. Keeps caps single source.
///
/// Keep in sync with `scripts/_mikrotik_shared.py::validate_host` /
/// `format_host_for_url` / `resolve_scheme`.
pub(crate) fn build_base_url_with_allow(
    host: &str,
    port: u16,
    scheme: &str,
    allow_loopback: bool,
) -> Result<url::Url, LiveError> {
    validate_host_with_allow(host, allow_loopback)?;
    if port == 0 {
        return Err(LiveError::InvalidPort("port 0".to_string()));
    }
    if host.contains('/') || host.contains('\\') {
        return Err(LiveError::InvalidHost(
            "host contains path separator".to_string(),
        ));
    }
    if is_ssrf_denied_host(host) {
        return Err(LiveError::InvalidHost("SSRF denied host".to_string()));
    }
    // Loopback/private check already done via validate_host_with_allow above, but
    // keep SSRF deny above for explicitness.
    let host_for_url = format_host_for_url(host);
    let url_str = format!("{scheme}://{host_for_url}:{port}/");
    let parsed = url::Url::parse(&url_str)
        .map_err(|e| LiveError::InvalidHost(format!("invalid url: {e}")))?;
    if parsed.scheme() != scheme {
        return Err(LiveError::InvalidHost("scheme mismatch".to_string()));
    }
    Ok(parsed)
}

/// Build and validate the REST URL for a given resource.
///
/// Uses `build_base_url_with_allow` for shared validation, then appends the resource path.
/// Handles IPv6 bracket wrapping via `format_host_for_url`.
pub(crate) fn build_rest_url(
    config: &LiveConfig,
    resource: ResourceKind,
) -> Result<String, LiveError> {
    let mut base = build_base_url_with_allow(
        &config.host,
        config.port,
        config.scheme(),
        config.allow_loopback,
    )?;
    base.set_path(resource.rest_path());
    let url_str = base.to_string();
    // Re-validate full URL (scheme + host + path) via Url crate.
    let parsed = url::Url::parse(&url_str)
        .map_err(|e| LiveError::InvalidHost(format!("invalid url: {e}")))?;
    if parsed.scheme() != config.scheme() {
        return Err(LiveError::InvalidHost("scheme mismatch".to_string()));
    }
    Ok(url_str)
}

/// Build URL for a custom resource.
fn build_custom_rest_url(
    config: &LiveConfig,
    custom: &CustomResource,
) -> Result<String, LiveError> {
    let mut base = build_base_url_with_allow(
        &config.host,
        config.port,
        config.scheme(),
        config.allow_loopback,
    )?;
    // Ensure custom path starts with /
    let path = if custom.path.starts_with('/') {
        custom.path.clone()
    } else {
        format!("/{}", custom.path)
    };
    base.set_path(&path);
    let url_str = base.to_string();
    let parsed = url::Url::parse(&url_str)
        .map_err(|e| LiveError::InvalidHost(format!("invalid url: {e}")))?;
    if parsed.scheme() != config.scheme() {
        return Err(LiveError::InvalidHost("scheme mismatch".to_string()));
    }
    // F7: re-validate the parsed path — `Url::set_path` normalizes
    // percent-encoding and dot-segments, so the allowlist must hold on the
    // final parsed form, not just the raw configured string.
    if parsed.path() != "/rest" && !parsed.path().starts_with("/rest/") {
        return Err(LiveError::InvalidHost(
            "custom path escapes /rest".to_string(),
        ));
    }
    Ok(url_str)
}

// ── LiveError ────────────────────────────────────────────────────

/// Errors from live fetching, never containing `pass`.
#[derive(Debug, Clone)]
pub enum LiveError {
    /// Live is disabled (opt-in not set or missing host/pass).
    Disabled,
    InvalidHost(String),
    InvalidPort(String),
    /// Network / transport error (sanitized, no pass).
    Network(String),
    /// HTTP status error.
    Status(u16),
    /// Response exceeded `MAX_LIVE_RESPONSE_BYTES`.
    ResponseTooLarge(usize),
    /// JSON parse or shape error.
    Parse(String),
    /// Request timed out.
    Timeout,
}

impl std::fmt::Display for LiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "live disabled"),
            Self::InvalidHost(r) => write!(f, "invalid host: {r}"),
            Self::InvalidPort(r) => write!(f, "invalid port: {r}"),
            Self::Network(msg) => write!(f, "network error: {msg}"),
            Self::Status(code) => write!(f, "http status {code}"),
            Self::ResponseTooLarge(n) => write!(f, "response too large ({n} bytes)"),
            Self::Parse(msg) => write!(f, "parse error: {msg}"),
            Self::Timeout => write!(f, "request timed out"),
        }
    }
}

impl std::error::Error for LiveError {}

// ── ResourceKind & Value filtering ────────────────────────────────

/// Kinds of live RouterOS resources enrichable over REST.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceKind {
    Interfaces,
    IpAddresses,
    Ipv6Addresses,
    AddressLists,
    Ipv6AddressLists,
    FirewallFilterChains,
    FirewallMangleChains,
    FirewallNatChains,
    FirewallRawChains,
    IpPools,
    Ipv6Pools,
}

impl ResourceKind {
    /// Return slice of all supported live resource kinds.
    #[cfg(test)]
    pub fn all() -> &'static [ResourceKind] {
        &[
            ResourceKind::Interfaces,
            ResourceKind::IpAddresses,
            ResourceKind::Ipv6Addresses,
            ResourceKind::AddressLists,
            ResourceKind::Ipv6AddressLists,
            ResourceKind::FirewallFilterChains,
            ResourceKind::FirewallMangleChains,
            ResourceKind::FirewallNatChains,
            ResourceKind::FirewallRawChains,
            ResourceKind::IpPools,
            ResourceKind::Ipv6Pools,
        ]
    }

    /// Cache key in `LiveCache`.
    pub fn cache_key(&self) -> &'static str {
        match self {
            Self::Interfaces => "interfaces",
            Self::IpAddresses => "ip_addresses",
            Self::Ipv6Addresses => "ipv6_addresses",
            Self::AddressLists => "address_lists",
            Self::Ipv6AddressLists => "ipv6_address_lists",
            Self::FirewallFilterChains => "firewall_filter_chains",
            Self::FirewallMangleChains => "firewall_mangle_chains",
            Self::FirewallNatChains => "firewall_nat_chains",
            Self::FirewallRawChains => "firewall_raw_chains",
            Self::IpPools => "ip_pools",
            Self::Ipv6Pools => "ipv6_pools",
        }
    }

    /// REST path on RouterOS.
    pub fn rest_path(&self) -> &'static str {
        match self {
            Self::Interfaces => "/rest/interface",
            Self::IpAddresses => "/rest/ip/address",
            Self::Ipv6Addresses => "/rest/ipv6/address",
            Self::AddressLists => "/rest/ip/firewall/address-list",
            Self::Ipv6AddressLists => "/rest/ipv6/firewall/address-list",
            Self::FirewallFilterChains => "/rest/ip/firewall/filter",
            Self::FirewallMangleChains => "/rest/ip/firewall/mangle",
            Self::FirewallNatChains => "/rest/ip/firewall/nat",
            Self::FirewallRawChains => "/rest/ip/firewall/raw",
            Self::IpPools => "/rest/ip/pool",
            Self::Ipv6Pools => "/rest/ipv6/pool",
        }
    }

    /// Primary JSON field name extracted from array items.
    pub fn json_field(&self) -> &'static str {
        match self {
            Self::Interfaces => "name",
            Self::IpAddresses | Self::Ipv6Addresses => "address",
            Self::AddressLists | Self::Ipv6AddressLists => "list",
            Self::FirewallFilterChains
            | Self::FirewallMangleChains
            | Self::FirewallNatChains
            | Self::FirewallRawChains => "chain",
            Self::IpPools | Self::Ipv6Pools => "name",
        }
    }

    /// LSP completion item detail string.
    pub fn detail_label(&self) -> &'static str {
        match self {
            Self::Interfaces => "live — interface on device",
            Self::IpAddresses => "live — IPv4 address on device",
            Self::Ipv6Addresses => "live — IPv6 address on device",
            Self::AddressLists => "live — firewall address-list",
            Self::Ipv6AddressLists => "live — IPv6 firewall address-list",
            Self::FirewallFilterChains => "live — firewall filter chain",
            Self::FirewallMangleChains => "live — firewall mangle chain",
            Self::FirewallNatChains => "live — firewall NAT chain",
            Self::FirewallRawChains => "live — firewall raw chain",
            Self::IpPools => "live — IP pool on device",
            Self::Ipv6Pools => "live — IPv6 pool on device",
        }
    }

    /// Filter and sanitize a single raw value for this resource kind.
    pub fn filter_raw_value(&self, raw: &str) -> Option<String> {
        match self {
            Self::IpAddresses | Self::Ipv6Addresses => filter_ip_value(raw),
            _ => filter_value(raw),
        }
    }
}

/// Whether `c` is allowed in a live identifier value (alphanumeric, '-', '_', or '.').
fn is_valid_value_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'
}

/// Whether `c` is allowed in a live IP/prefix value.
fn is_valid_ip_char(c: char) -> bool {
    c.is_ascii_hexdigit() || c == '.' || c == ':' || c == '/'
}

/// Validate and sanitize a single live identifier value.
///
/// - non-empty, length <= `MAX_LIVE_VALUE_LEN`
/// - only allowed chars, no control/null
/// - trimmed
pub(crate) fn filter_value(raw: &str) -> Option<String> {
    // Reject null and control chars in the raw input (including those that
    // `trim()` would otherwise strip, e.g. trailing newline) — the only
    // whitespace tolerated for trimming is ASCII space.
    if raw.contains('\0') {
        return None;
    }
    if raw.chars().any(|c| c.is_control()) {
        return None;
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() > MAX_LIVE_VALUE_LEN {
        return None;
    }
    if !trimmed.chars().all(is_valid_value_char) {
        return None;
    }
    Some(trimmed.to_string())
}

/// Validate and sanitize a single live IP address or prefix value.
pub(crate) fn filter_ip_value(raw: &str) -> Option<String> {
    if raw.contains('\0') {
        return None;
    }
    if raw.chars().any(|c| c.is_control()) {
        return None;
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() > MAX_LIVE_VALUE_LEN {
        return None;
    }
    if !trimmed.chars().all(is_valid_ip_char) {
        return None;
    }
    Some(trimmed.to_string())
}

/// Filter, deduplicate, sort, and cap a list of raw values for a specific resource.
pub(crate) fn sanitize_resource_values(raw: Vec<String>, resource: ResourceKind) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for v in raw {
        if let Some(clean) = resource.filter_raw_value(&v)
            && seen.insert(clean.clone())
        {
            out.push(clean);
        }
        if out.len() >= MAX_LIVE_ITEMS {
            break;
        }
    }
    out.sort();
    if out.len() > MAX_LIVE_ITEMS {
        out.truncate(MAX_LIVE_ITEMS);
    }
    out
}

/// Filter, deduplicate, sort, and cap a list of raw values (default interface kind).
#[cfg(test)]
pub(crate) fn sanitize_values(raw: Vec<String>) -> Vec<String> {
    sanitize_resource_values(raw, ResourceKind::Interfaces)
}

// ── Cache ────────────────────────────────────────────────────────

/// One cached live collection.
#[derive(Clone, Debug)]
pub struct CachedValue {
    pub values: Arc<[String]>,
    pub fetched_at: Instant,
}

/// In-memory live cache with TTL and entry cap.
///
/// Key is the collection name (e.g. `"interfaces"`, `"ip_addresses"`).
#[derive(Debug)]
pub struct LiveCache {
    pub entries: HashMap<String, CachedValue>,
    pub ttl: Duration,
    /// Last failure times for negative cache (avoid immediate retry spam).
    pub failed_at: HashMap<String, Instant>,
    /// Last fetch attempt times for coalescing (avoid spawning parallel fetches).
    pub last_fetch_attempt: HashMap<String, Instant>,
}

impl LiveCache {
    /// Create a cache with explicit TTL.
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            ttl,
            failed_at: HashMap::new(),
            last_fetch_attempt: HashMap::new(),
        }
    }

    /// Create a cache with the default TTL (`LIVE_TTL_SECS`).
    pub fn with_default_ttl() -> Self {
        Self::new(Duration::from_secs(LIVE_TTL_SECS))
    }

    /// Whether an entry fetched at `fetched_at` is still fresh.
    fn is_fresh(&self, fetched_at: Instant) -> bool {
        fetched_at.elapsed() < self.ttl
    }

    /// Non-blocking read: return a cloned Arc if the entry is fresh (cheap, no 500-item Vec clone per keystroke).
    pub fn try_get_cached(&self, key: &str) -> Option<Arc<[String]>> {
        let entry = self.entries.get(key)?;
        if self.is_fresh(entry.fetched_at) {
            Some(Arc::clone(&entry.values))
        } else {
            None
        }
    }

    /// Whether a key is in negative cooldown (recent failure, within `LIVE_NEGATIVE_TTL_SECS`).
    pub fn is_negative_cooldown(&self, key: &str) -> bool {
        if let Some(at) = self.failed_at.get(key) {
            at.elapsed() < Duration::from_secs(LIVE_NEGATIVE_TTL_SECS)
        } else {
            false
        }
    }

    /// Whether a fetch can be spawned for `key` (not coalesced and not in negative cooldown).
    pub fn can_spawn_fetch(&self, key: &str) -> bool {
        if self.is_negative_cooldown(key) {
            return false;
        }
        if let Some(last) = self.last_fetch_attempt.get(key)
            && last.elapsed() < Duration::from_secs(LIVE_FETCH_BLOCKING_TIMEOUT_SECS)
        {
            return false;
        }
        true
    }

    /// Record a fetch attempt for coalescing.
    pub fn record_fetch_attempt(&mut self, key: String) {
        self.last_fetch_attempt.insert(key, Instant::now());
    }

    /// Insert or replace a cache entry, enforcing caps.
    pub fn insert(&mut self, key: String, values: Vec<String>) {
        let mut vals = values;
        if vals.len() > MAX_LIVE_ITEMS {
            vals.truncate(MAX_LIVE_ITEMS);
        }
        // Defensive: also cap value lengths (should already be filtered).
        vals.retain(|v| v.len() <= MAX_LIVE_VALUE_LEN);
        // Evict oldest if at capacity and inserting a new key.
        if !self.entries.contains_key(&key)
            && self.entries.len() >= MAX_CACHE_ENTRIES
            && let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, v)| v.fetched_at)
                .map(|(k, _)| k.clone())
        {
            self.entries.remove(&oldest_key);
            log_debug!("live cache evicted oldest key {oldest_key:?} at cap {MAX_CACHE_ENTRIES}");
        }
        let arc: Arc<[String]> = Arc::from(vals.into_boxed_slice());
        self.entries.insert(
            key.clone(),
            CachedValue {
                values: arc,
                fetched_at: Instant::now(),
            },
        );
        // Success clears negative cooldown for this key.
        self.failed_at.remove(&key);
    }

    /// Insert a negative cache entry (failure) to avoid immediate retry spam.
    pub fn insert_negative(&mut self, key: String) {
        let key_clone = key.clone();
        self.failed_at.insert(key, Instant::now());
        log_debug!(
            "live negative cooldown inserted for {key_clone:?} ttl={}s",
            LIVE_NEGATIVE_TTL_SECS
        );
    }

    /// Clear a single cache entry and its negative state.
    pub fn clear_key(&mut self, key: &str) {
        self.entries.remove(key);
        self.failed_at.remove(key);
        self.last_fetch_attempt.remove(key);
    }

    /// Clear all entries and negative state.
    pub fn clear_all(&mut self) {
        self.entries.clear();
        self.failed_at.clear();
        self.last_fetch_attempt.clear();
    }

    /// Test helper: insert with explicit `fetched_at` (for TTL tests).
    #[cfg(test)]
    pub(crate) fn insert_with_time(&mut self, key: String, values: Vec<String>, at: Instant) {
        let mut vals = values;
        if vals.len() > MAX_LIVE_ITEMS {
            vals.truncate(MAX_LIVE_ITEMS);
        }
        vals.retain(|v| v.len() <= MAX_LIVE_VALUE_LEN);
        if !self.entries.contains_key(&key)
            && self.entries.len() >= MAX_CACHE_ENTRIES
            && let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, v)| v.fetched_at)
                .map(|(k, _)| k.clone())
        {
            self.entries.remove(&oldest_key);
        }
        let arc: Arc<[String]> = Arc::from(vals.into_boxed_slice());
        self.entries.insert(
            key.clone(),
            CachedValue {
                values: arc,
                fetched_at: at,
            },
        );
        // Clear negative on explicit insert for test determinism.
        self.failed_at.remove(&key);
    }
}

/// Match menu path, property name and argument type to the corresponding live resource kind.
pub fn live_resource_for_menu_property(
    menu_path: &str,
    property: &str,
    type_str: &str,
) -> Option<ResourceKind> {
    let path_low = menu_path.to_ascii_lowercase();
    let prop_low = property.to_ascii_lowercase();
    let type_low = type_str.to_ascii_lowercase();

    let is_ipv6 = path_low.starts_with("/ipv6") || type_low.contains("ipv6");

    // 1. Interfaces / bridges / ports
    if matches!(
        prop_low.as_str(),
        "interface"
            | "bridge"
            | "actual-interface"
            | "parent"
            | "in-interface"
            | "out-interface"
            | "in-interface-list"
            | "out-interface-list"
            | "master-interface"
    ) || type_low.contains("iface")
    {
        return Some(ResourceKind::Interfaces);
    }

    // 2. Firewall address-list (IPv4 vs IPv6)
    if matches!(
        prop_low.as_str(),
        "src-address-list" | "dst-address-list" | "address-list" | "list"
    ) {
        if is_ipv6 {
            return Some(ResourceKind::Ipv6AddressLists);
        } else {
            return Some(ResourceKind::AddressLists);
        }
    }

    // 3. Firewall chains (filter, mangle, nat, raw)
    if matches!(prop_low.as_str(), "chain" | "jump-target") {
        if path_low.contains("mangle") {
            return Some(ResourceKind::FirewallMangleChains);
        } else if path_low.contains("nat") {
            return Some(ResourceKind::FirewallNatChains);
        } else if path_low.contains("raw") {
            return Some(ResourceKind::FirewallRawChains);
        } else {
            return Some(ResourceKind::FirewallFilterChains);
        }
    }

    // 4. IP Pools (IPv4 vs IPv6)
    if matches!(
        prop_low.as_str(),
        "address-pool" | "pool" | "pool-name" | "remote-pool"
    ) || type_low.contains("pool")
    {
        if is_ipv6 {
            return Some(ResourceKind::Ipv6Pools);
        } else {
            return Some(ResourceKind::IpPools);
        }
    }

    // 5. IP Addresses / prefixes / gateways (IPv4 vs IPv6)
    if matches!(
        prop_low.as_str(),
        "address"
            | "network"
            | "src-address"
            | "dst-address"
            | "gateway"
            | "target-addresses"
            | "to-addresses"
            | "local-address"
            | "remote-address"
    ) || type_low.starts_with("ipaddr")
        || type_low.starts_with("ipprefix")
        || type_low == "address"
    {
        if is_ipv6 {
            return Some(ResourceKind::Ipv6Addresses);
        } else {
            return Some(ResourceKind::IpAddresses);
        }
    }

    None
}

/// Match property name and argument type to the corresponding live resource kind.
#[cfg(test)]
pub fn live_resource_for_property(property: &str, type_str: &str) -> Option<ResourceKind> {
    live_resource_for_menu_property("", property, type_str)
}

/// Whether the property `property`/`type_str` is live-enrichable.
#[cfg(test)]
pub fn is_live_property(property: &str, type_str: &str) -> bool {
    let prop_low = property.to_ascii_lowercase();
    if matches!(
        prop_low.as_str(),
        "interface" | "bridge" | "actual-interface"
    ) {
        return true;
    }
    if type_str.to_ascii_lowercase().contains("iface") {
        return true;
    }
    live_resource_for_property(property, type_str).is_some()
}

/// Return live values for `property`/`type_str` if the cache is live-enrichable.
#[cfg(test)]
pub fn live_values_for_property(
    cache: &LiveCache,
    property: &str,
    type_str: &str,
) -> Option<Arc<[String]>> {
    let res = live_resource_for_property(property, type_str)?;
    cache.try_get_cached(res.cache_key())
}

/// Return live resource kind and values for `property`/`type_str` if the cache is live-enrichable.
pub fn live_resource_values_for_property(
    cache: &LiveCache,
    menu_path: &str,
    property: &str,
    type_str: &str,
) -> Option<(ResourceKind, Arc<[String]>)> {
    let res = live_resource_for_menu_property(menu_path, property, type_str)?;
    let vals = cache.try_get_cached(res.cache_key())?;
    Some((res, vals))
}

/// Non-blocking stale-while-revalidate: only read cache, trigger background fetch if needed.
///
/// Returns cached values if fresh, otherwise `None` (caller should use honest static set).
/// If miss/stale and not in cooldown/coalesced, spawns a background thread to fetch.
pub fn get_cached_or_fetch_background(
    cache: &Arc<Mutex<LiveCache>>,
    config: &LiveConfig,
    resource: ResourceKind,
) -> Option<Arc<[String]>> {
    if !config.is_active() {
        return None;
    }
    let key = resource.cache_key().to_string();
    // Fast path: fresh cache.
    {
        let guard = cache.lock().unwrap_or_else(|e| {
            log_warn!("live cache lock poisoned, recovering");
            e.into_inner()
        });
        if let Some(vals) = guard.try_get_cached(&key) {
            log_debug!("live cache hit (fresh) for {key}");
            return Some(vals);
        }
        if guard.is_negative_cooldown(&key) {
            log_debug!("live negative cooldown for {key}, skipping fetch");
            return None;
        }
        if !guard.can_spawn_fetch(&key) {
            log_debug!("live fetch coalesced for {:?} key={}", resource, key);
            return None;
        }
    }
    // Record attempt before spawning to coalesce concurrent callers.
    {
        let mut guard = cache.lock().unwrap_or_else(|e| {
            log_warn!("live cache lock poisoned, recovering");
            e.into_inner()
        });
        // Re-check after acquiring write lock (avoid TOCTOU).
        if !guard.can_spawn_fetch(&key) {
            return None;
        }
        if guard.try_get_cached(&key).is_some() {
            return guard.try_get_cached(&key);
        }
        guard.record_fetch_attempt(key.clone());
    }
    trigger_background_fetch(cache, config, resource, key);
    None
}

fn trigger_background_fetch(
    cache: &Arc<Mutex<LiveCache>>,
    config: &LiveConfig,
    resource: ResourceKind,
    key: String,
) {
    if !try_acquire_fetch_permit() {
        log_debug!(
            "live fetch semaphore full (cap={MAX_CONCURRENT_FETCHES}), skipping fetch for {resource:?} key={key}"
        );
        return;
    }
    let cache_clone = Arc::clone(cache);
    let config_clone = config.clone();
    log_debug!("live background fetch triggered for {:?}", resource);
    std::thread::spawn(move || {
        let _guard = FetchPermitGuard;
        let start = Instant::now();
        let result = fetch_resource(&config_clone, resource);
        let elapsed = start.elapsed();
        match result {
            Ok(values) => {
                if values.is_empty() {
                    log_debug!("live fetch {:?} returned empty set", resource);
                    // Cache empty results so repeated completions against an
                    // endpoint returning `[]` hit the fresh empty entry instead
                    // of re-hitting the network every time.
                    let mut guard = cache_clone.lock().unwrap_or_else(|e| {
                        log_warn!("live cache lock poisoned, recovering");
                        e.into_inner()
                    });
                    guard.insert(key.clone(), values);
                    return;
                }
                log_info!(
                    "live fetch ok kind={:?} host={} latency_ms={} items={}",
                    resource,
                    sanitize_for_log(&config_clone.host),
                    elapsed.as_millis(),
                    values.len()
                );
                let mut guard = cache_clone.lock().unwrap_or_else(|e| {
                    log_warn!("live cache lock poisoned, recovering");
                    e.into_inner()
                });
                guard.insert(key.clone(), values);
            }
            Err(e) => {
                // F6: belt-and-braces — Network is redacted at construction,
                // but redact again at the log boundary in case a future
                // error variant echoes request context.
                let safe = redact_secrets(&e.to_string(), &config_clone.pass, &config_clone.user);
                log_warn!(
                    "live fetch {:?} failed: {} latency_ms={} host={}",
                    resource,
                    safe,
                    elapsed.as_millis(),
                    sanitize_for_log(&config_clone.host)
                );
                let mut guard = cache_clone.lock().unwrap_or_else(|e| {
                    log_warn!("live cache lock poisoned, recovering");
                    e.into_inner()
                });
                guard.insert_negative(key.clone());
            }
        }
    });
}

/// Trigger live enrichment for a completion request (stale-while-revalidate).
///
/// Called by the `textDocument/completion` handler once per request; it
/// encapsulates what the handler used to inline around live data:
///
/// 1. Custom match — when `property` names a custom resource, ONLY the
///    custom key `custom:<property>` is fetched; the generic Interfaces
///    prefetch is skipped to avoid a double fetch.
/// 2. Built-in background fetch — `resolve_resource_with_custom` on (menu
///    path, property, arg type); when `property` is `None` (cursor not
///    inside a `key=value` assignment) interfaces are prefetched as the
///    likely target. Interfaces prefetch only for interface-like/empty
///    context: an unresolvable property fetches nothing.
/// 3. Coalescing-aware spawn via `get_cached_or_fetch_background`
///    (`try_get_cached`, `is_negative_cooldown`, `can_spawn_fetch`,
///    `record_fetch_attempt`) or `trigger_custom_fetch_background` for the
///    custom key (never collides with built-in entries).
///
/// Never blocks and never logs `pass`. `context_path` is the menu path of
/// the line being completed; `arg_type` is the menu-declared argument type
/// for `property`, or `""` when unknown.
pub fn trigger_enrichment_for_completion(
    cache: &Arc<Mutex<LiveCache>>,
    config: &LiveConfig,
    property: Option<&str>,
    context_path: &str,
    arg_type: &str,
) {
    // Custom resource wins alone: fetch ONLY the custom key, skip the
    // generic Interfaces prefetch (avoids a double fetch per keystroke).
    if let Some(key) = property
        && let Some(custom) = config.custom_resource_for_property(key).cloned()
    {
        trigger_custom_fetch_background(cache, config, &custom);
        return;
    }

    let target_resource = match property {
        Some(key) => config.resolve_resource_with_custom(context_path, key, arg_type),
        None => Some(ResourceKind::Interfaces),
    };

    if let Some(res) = target_resource {
        let _ = get_cached_or_fetch_background(cache, config, res);
        log_debug!("live background fetch triggered for {res:?}");
    }
    // Else: property has no known resource — fetch nothing. Interfaces are
    // prefetched only for interface-like/empty context, not as a generic
    // fallback for unrelated properties.
}

/// Spawn a coalescing-aware background fetch for a custom resource.
///
/// Cache key is `custom:<property>`. Mirrors `get_cached_or_fetch_background`
/// semantics: a fresh cache entry or a negative cooldown short-circuits, and
/// at most one fetch is in flight per key within
/// `LIVE_FETCH_BLOCKING_TIMEOUT_SECS`. Empty results are cached as fresh
/// empty entries (same as the built-in path) so endpoints returning `[]`
/// do not re-hit the network on every completion.
fn trigger_custom_fetch_background(
    cache: &Arc<Mutex<LiveCache>>,
    config: &LiveConfig,
    custom: &CustomResource,
) {
    let key = format!("custom:{}", custom.property);
    // Fast path: fresh cache, negative cooldown, or in-flight fetch.
    {
        let guard = cache.lock().unwrap_or_else(|e| {
            log_warn!("live cache lock poisoned, recovering");
            e.into_inner()
        });
        if guard.try_get_cached(&key).is_some() {
            log_debug!("live cache hit (fresh) for {key}");
            return;
        }
        if guard.is_negative_cooldown(&key) {
            log_debug!("live negative cooldown for {key}, skipping custom fetch");
            return;
        }
        if !guard.can_spawn_fetch(&key) {
            log_debug!("live custom fetch coalesced for {key}");
            return;
        }
    }
    // Record the attempt before spawning to coalesce concurrent callers
    // (re-check under the write lock to avoid a TOCTOU double spawn).
    {
        let mut guard = cache.lock().unwrap_or_else(|e| {
            log_warn!("live cache lock poisoned, recovering");
            e.into_inner()
        });
        if !guard.can_spawn_fetch(&key) {
            return;
        }
        guard.record_fetch_attempt(key.clone());
    }

    if !try_acquire_fetch_permit() {
        log_debug!(
            "live custom fetch semaphore full (cap={MAX_CONCURRENT_FETCHES}), skipping custom fetch for {key}"
        );
        return;
    }
    let cache_clone = Arc::clone(cache);
    let config_clone = config.clone();
    let custom_clone = custom.clone();
    let key_clone = key.clone();
    log_debug!("live background fetch triggered for custom resource {key_clone}");
    std::thread::spawn(move || {
        let _guard = FetchPermitGuard;
        let start = Instant::now();
        match fetch_custom_resource(&config_clone, &custom_clone) {
            Ok(vals) => {
                if vals.is_empty() {
                    log_debug!(
                        "live custom fetch {} returned empty set",
                        sanitize_for_log(&custom_clone.property)
                    );
                    // Cache empty results (see built-in path above).
                    let mut guard = cache_clone.lock().unwrap_or_else(|e| {
                        log_warn!("live cache lock poisoned, recovering");
                        e.into_inner()
                    });
                    guard.insert(key_clone, vals);
                    return;
                }
                log_info!(
                    "live fetch ok custom property={} path={} latency_ms={} items={}",
                    sanitize_for_log(&custom_clone.property),
                    sanitize_for_log(&custom_clone.path),
                    start.elapsed().as_millis(),
                    vals.len()
                );
                let mut guard = cache_clone.lock().unwrap_or_else(|e| {
                    log_warn!("live cache lock poisoned, recovering");
                    e.into_inner()
                });
                guard.insert(key_clone, vals);
            }
            Err(e) => {
                let safe = redact_secrets(&e.to_string(), &config_clone.pass, &config_clone.user);
                log_warn!(
                    "live fetch custom failed property={} path={} err={} latency_ms={}",
                    sanitize_for_log(&custom_clone.property),
                    sanitize_for_log(&custom_clone.path),
                    safe,
                    start.elapsed().as_millis()
                );
                let mut guard = cache_clone.lock().unwrap_or_else(|e| {
                    log_warn!("live cache lock poisoned, recovering");
                    e.into_inner()
                });
                guard.insert_negative(key_clone);
            }
        }
    });
}

// ── Fetch ────────────────────────────────────────────────────────

/// Get a cached `ureq::Agent` for the given timeout and TLS verification mode, or build a new one.
///
/// Uses a global `OnceLock` cache keyed by `(timeout_secs, ssl_verify)` to reuse agents across calls.
/// Logs `live agent reuse` on hit. Prefer `get_cached_agent_for_config` (pin/CA aware); this
/// wrapper exists for unit tests and pin-less call sites.
pub(crate) fn get_cached_agent(timeout: Duration, ssl_verify: bool) -> ureq::Agent {
    static AGENT_CACHE: OnceLock<Mutex<HashMap<(u64, bool), ureq::Agent>>> = OnceLock::new();
    let cache = AGENT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (timeout.as_secs(), ssl_verify);
    {
        let guard = cache.lock().unwrap_or_else(|e| {
            log_warn!("agent cache lock poisoned, recovering");
            e.into_inner()
        });
        if let Some(agent) = guard.get(&key) {
            log_debug!(
                "live agent reuse timeout={}s ssl_verify={} ssl_verify_effective={}",
                key.0,
                key.1,
                key.1
            );
            return agent.clone();
        }
    }
    // Build new agent
    // Redirects are disabled (`.redirects(0)`): the code treats 3xx as
    // `LiveError::Status`, so nothing is lost and open-redirect SSRF is closed.
    let agent = if ssl_verify {
        ureq::AgentBuilder::new()
            .timeout(timeout)
            .redirects(0)
            .build()
    } else {
        log_warn!(
            "live ssl_verify=false — building agent with insecure TLS verifier (host verification disabled)"
        );
        match build_insecure_agent(timeout) {
            Some(a) => a,
            None => {
                log_warn!(
                    "live insecure agent build failed, falling back to default verifier (verification will still be attempted)"
                );
                ureq::AgentBuilder::new()
                    .timeout(timeout)
                    .redirects(0)
                    .build()
            }
        }
    };
    {
        let mut guard = cache.lock().unwrap_or_else(|e| {
            log_warn!("agent cache lock poisoned, recovering");
            e.into_inner()
        });
        guard.insert(key, agent.clone());
    }
    agent
}

/// Pin-aware agent cache key: timeout + effective verify + pin + CA path.
fn agent_cache_key_for_config(
    config: &LiveConfig,
    timeout: Duration,
) -> (u64, bool, Option<[u8; 32]>, String) {
    (
        timeout.as_secs(),
        config.ssl_verify_effective(),
        config.fingerprint,
        config.ca_file.clone(),
    )
}

/// Get (or build) the `ureq::Agent` for a full `LiveConfig`.
///
/// Selection order on `https`:
///
/// 1. Pin set (+ optional custom CA roots) => SPKI-pinning verifier.
/// 2. Custom CA file only => chain verifier against those roots.
/// 3. `ssl_verify_effective()` true => default platform roots.
/// 4. Otherwise => insecure verifier + WARN (preserves `MIKROTIK_SSL=0`).
///
/// On `http` no TLS is involved; a plain agent is returned. Redirects are
/// always disabled (3xx surfaces as `LiveError::Status`).
pub(crate) fn get_cached_agent_for_config(config: &LiveConfig, timeout: Duration) -> ureq::Agent {
    type PinnedAgentCacheKey = (u64, bool, Option<[u8; 32]>, String);
    static PINNED_CACHE: OnceLock<Mutex<HashMap<PinnedAgentCacheKey, ureq::Agent>>> =
        OnceLock::new();
    let cache = PINNED_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = agent_cache_key_for_config(config, timeout);
    {
        let guard = cache.lock().unwrap_or_else(|e| {
            log_warn!("agent cache lock poisoned, recovering");
            e.into_inner()
        });
        if let Some(agent) = guard.get(&key) {
            log_debug!(
                "live agent reuse timeout={}s ssl_verify_effective={} pin_set={} ca_set={}",
                key.0,
                key.1,
                key.2.is_some(),
                !key.3.is_empty()
            );
            return agent.clone();
        }
    }
    let agent = build_agent_for_config(config, timeout);
    {
        let mut guard = cache.lock().unwrap_or_else(|e| {
            log_warn!("agent cache lock poisoned, recovering");
            e.into_inner()
        });
        guard.insert(key, agent.clone());
    }
    agent
}

/// Build (uncached) the agent for a config; fail-closed fallbacks never
/// silently downgrade to insecure: on pin/CA build failure a default
/// verifying agent is returned so the handshake still validates.
fn build_agent_for_config(config: &LiveConfig, timeout: Duration) -> ureq::Agent {
    if config.scheme() != "https" {
        return ureq::AgentBuilder::new()
            .timeout(timeout)
            .redirects(0)
            .build();
    }
    if let Some(pin) = config.fingerprint {
        if !config.ca_file.is_empty() {
            match build_pinned_with_ca_agent(timeout, pin, &config.ca_file) {
                Some(a) => {
                    log_info!("live tls pin + custom CA active");
                    return a;
                }
                None => {
                    log_warn!(
                        "live pin/CA agent build failed, falling back to default verifier (fail-closed, verification still attempted)"
                    );
                    return ureq::AgentBuilder::new()
                        .timeout(timeout)
                        .redirects(0)
                        .build();
                }
            }
        }
        match build_pinned_agent(timeout, pin) {
            Some(a) => {
                log_info!("live tls SPKI pin active (chain replaced by pin check)");
                return a;
            }
            None => {
                log_warn!(
                    "live pinned agent build failed, falling back to default verifier (fail-closed)"
                );
                return ureq::AgentBuilder::new()
                    .timeout(timeout)
                    .redirects(0)
                    .build();
            }
        }
    }
    if !config.ca_file.is_empty() {
        match build_ca_agent(timeout, &config.ca_file) {
            Some(a) => {
                log_info!("live custom CA active");
                return a;
            }
            None => {
                log_warn!(
                    "live custom CA agent build failed, falling back to default verifier (fail-closed)"
                );
                return ureq::AgentBuilder::new()
                    .timeout(timeout)
                    .redirects(0)
                    .build();
            }
        }
    }
    // No pin/CA: reuse the legacy path (secure default or insecure + WARN).
    get_cached_agent(timeout, config.ssl_verify_effective())
}

/// SPKI-pinning verifier: accepts only a leaf whose SPKI SHA256 equals `pin`.
///
/// Chain validation is replaced by the pin check (TOFU-style pinning, no new
/// dependency on a roots bundle). Any mismatch or malformed cert fails the
/// handshake fail-closed. Signatures are asserted because authenticity is
/// bound to the pin itself.
#[derive(Debug)]
struct SpkiPinVerifier {
    pin: [u8; 32],
}

impl rustls::client::danger::ServerCertVerifier for SpkiPinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        match spki_sha256(end_entity.as_ref()) {
            Some(digest) if digest == self.pin => {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            _ => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Build an agent that pins the leaf SPKI instead of chain validation.
fn build_pinned_agent(timeout: Duration, pin: [u8; 32]) -> Option<ureq::Agent> {
    let provider = rustls::crypto::ring::default_provider();
    let tls_config = rustls::ClientConfig::builder_with_provider(provider.into())
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
        .ok()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SpkiPinVerifier { pin }))
        .with_no_client_auth();
    Some(
        ureq::AgentBuilder::new()
            .timeout(timeout)
            .redirects(0)
            .tls_config(Arc::new(tls_config))
            .build(),
    )
}

/// Parse PEM `CERTIFICATE` blocks from `pem_text` into DER certificates.
///
/// Minimal parser using the existing `base64` dependency (no new crates):
/// splits on BEGIN/END markers and base64-decodes each block. Non-certificate
/// blocks are skipped; returns the successfully decoded certs.
fn parse_pem_certs(pem_text: &str) -> Vec<rustls::pki_types::CertificateDer<'static>> {
    use base64::Engine;
    let mut out = Vec::new();
    let mut in_block = false;
    let mut b64 = String::new();
    for line in pem_text.lines() {
        let t = line.trim();
        if t == "-----BEGIN CERTIFICATE-----" {
            in_block = true;
            b64.clear();
            continue;
        }
        if t == "-----END CERTIFICATE-----" {
            if in_block {
                let compact: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
                if let Ok(der) = base64::engine::general_purpose::STANDARD.decode(&compact) {
                    out.push(rustls::pki_types::CertificateDer::from(der));
                } else {
                    log_warn!("live CA file: skipping undecodable PEM block");
                }
            }
            in_block = false;
            b64.clear();
            continue;
        }
        if in_block {
            b64.push_str(t);
        }
    }
    out
}

/// Build an agent validating chains against a user-provided PEM CA bundle.
///
/// Fail-closed `None` when the file is missing, unreadable, oversize
/// (>256 KiB), or contains no decodable certificates. The path itself is
/// sanitized in logs. Repeated failures for the same canonical path are
/// served from a negative cache (no re-read per keystroke).
fn build_ca_agent(timeout: Duration, ca_file: &str) -> Option<ureq::Agent> {
    let text = read_ca_bundle(ca_file)?;
    let certs = parse_pem_certs(&text);
    if certs.is_empty() {
        log_warn!(
            "live CA file has no decodable certificates: {:?}",
            sanitize_for_log(ca_file)
        );
        return None;
    }
    let mut roots = rustls::RootCertStore::empty();
    for cert in certs {
        if roots.add(cert).is_err() {
            log_warn!(
                "live CA file: skipping invalid certificate for {:?}",
                sanitize_for_log(ca_file)
            );
        }
    }
    if roots.is_empty() {
        return None;
    }
    let provider = rustls::crypto::ring::default_provider();
    let tls_config = rustls::ClientConfig::builder_with_provider(provider.into())
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
        .ok()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Some(
        ureq::AgentBuilder::new()
            .timeout(timeout)
            .redirects(0)
            .tls_config(Arc::new(tls_config))
            .build(),
    )
}

/// Combined verifier: chain must validate against the custom CA bundle AND
/// the leaf SPKI pin must match. Both checks fail-closed.
#[derive(Debug)]
struct PinnedWithCaVerifier {
    pin: [u8; 32],
    inner: Arc<dyn rustls::client::danger::ServerCertVerifier>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedWithCaVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;
        match spki_sha256(end_entity.as_ref()) {
            Some(digest) if digest == self.pin => {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            _ => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Build an agent combining custom CA chain validation with an SPKI pin.
fn build_pinned_with_ca_agent(
    timeout: Duration,
    pin: [u8; 32],
    ca_file: &str,
) -> Option<ureq::Agent> {
    let text = read_ca_bundle(ca_file)?;
    let certs = parse_pem_certs(&text);
    if certs.is_empty() {
        log_warn!(
            "live pin+CA file has no decodable certificates: {:?}",
            sanitize_for_log(ca_file)
        );
        return None;
    }
    let mut roots = rustls::RootCertStore::empty();
    for cert in certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return None;
    }
    let provider = rustls::crypto::ring::default_provider();
    let inner = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
        .build()
        .ok()?;
    let verifier = PinnedWithCaVerifier { pin, inner };
    let tls_config = rustls::ClientConfig::builder_with_provider(provider.into())
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
        .ok()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Some(
        ureq::AgentBuilder::new()
            .timeout(timeout)
            .redirects(0)
            .tls_config(Arc::new(tls_config))
            .build(),
    )
}

/// Build an agent that disables TLS verification (insecure).
///
/// Returns `None` if the rustls insecure config cannot be built.
pub(crate) fn build_insecure_agent(timeout: Duration) -> Option<ureq::Agent> {
    // Use rustls dangerous verifier that accepts any certificate.
    use rustls::DigitallySignedStruct;
    use rustls::SignatureScheme;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

    #[derive(Debug)]
    struct NoCertificateVerification;

    impl ServerCertVerifier for NoCertificateVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            // Use ring's supported schemes; provider is available via rustls crypto.
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    let provider = rustls::crypto::ring::default_provider();
    let tls_config = rustls::ClientConfig::builder_with_provider(provider.into())
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
        .ok()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoCertificateVerification))
        .with_no_client_auth();
    let agent = ureq::AgentBuilder::new()
        .timeout(timeout)
        .redirects(0)
        .tls_config(Arc::new(tls_config))
        .build();
    Some(agent)
}

/// Shared live-fetch path used by both `fetch_resource` and
/// `fetch_custom_resource`.
///
/// Performs the authenticated GET against `url` with the config's clamped
/// timeout, enforces the response caps from `caps.rs`
/// (`MAX_LIVE_RESPONSE_BYTES` via `reader.take(limit + 1)`), validates the
/// JSON-array shape, and extracts + sanitizes the values via
/// `extract_and_sanitize`.
///
/// `label` identifies the resource in logs (e.g. `Interfaces` or a custom
/// property name). Callers are responsible for `config.is_active()`, host
/// validation, and URL construction (via `build_base_url_with_allow`). `pass` is only
/// used in the Authorization header and never logged.
fn fetch_live_resource(
    config: &LiveConfig,
    url: &str,
    json_field: &str,
    kind: ResourceKind,
    label: &str,
) -> Result<Vec<String>, LiveError> {
    if !config.ssl_verify && config.fingerprint.is_none() && config.ca_file.is_empty() {
        log_warn!(
            "live ssl_verify=false — TLS verification disabled (insecure) scheme={} host={} port={} ssl_verify_effective={}",
            config.scheme(),
            sanitize_for_log(&config.host),
            config.port,
            config.ssl_verify_effective()
        );
    }
    if config.fingerprint_invalid {
        return Err(LiveError::Network(
            "invalid MIKROTIK_FINGERPRINT".to_string(),
        ));
    }
    // F1: resolve-then-revalidate at fetch time. Lexical checks ran at
    // config time; DNS may resolve differently now. Any denied resolved IP
    // (or resolution failure) fails closed before credentials are sent.
    // IP literals resolve locally without DNS traffic.
    resolve_and_validate_host(&config.host, config.port, config.allow_loopback)?;
    let timeout = Duration::from_secs(config.timeout_secs.clamp(1, 30));
    let agent = get_cached_agent_for_config(config, timeout);

    let start = Instant::now();
    let credentials = format!("{}:{}", config.user, config.pass);
    let encoded = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        credentials.as_bytes(),
    );
    let auth_header = format!("Basic {encoded}");

    let resp: Result<ureq::Response, ureq::Error> = agent
        .get(url)
        .set("Accept", "application/json")
        .set("Authorization", &auth_header)
        .call();

    let response: ureq::Response = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, _)) => return Err(LiveError::Status(code)),
        Err(ureq::Error::Transport(t)) => {
            let msg = t.to_string();
            if msg.to_ascii_lowercase().contains("timed out")
                || msg.to_ascii_lowercase().contains("timeout")
            {
                return Err(LiveError::Timeout);
            }
            // F6: transport errors echo URLs/headers on some stacks — strip
            // password and Basic material before storing.
            return Err(LiveError::Network(redact_secrets(
                &msg,
                &config.pass,
                &config.user,
            )));
        }
    };

    let status = response.status();
    if !(200..300).contains(&status) {
        return Err(LiveError::Status(status));
    }

    let reader = response.into_reader();
    let mut buf = Vec::new();
    let limit = MAX_LIVE_RESPONSE_BYTES + 1;
    let n = {
        use std::io::Read;
        let mut limited = reader.take(limit as u64);
        match limited.read_to_end(&mut buf) {
            Ok(n) => n,
            // F6: I/O errors may echo request context — redact before storing.
            Err(e) => {
                return Err(LiveError::Network(redact_secrets(
                    &e.to_string(),
                    &config.pass,
                    &config.user,
                )));
            }
        }
    };
    if n > MAX_LIVE_RESPONSE_BYTES {
        return Err(LiveError::ResponseTooLarge(n));
    }
    if buf.is_empty() {
        return Err(LiveError::Parse("empty response".to_string()));
    }

    let json: serde_json::Value =
        serde_json::from_slice(&buf).map_err(|e| LiveError::Parse(format!("invalid json: {e}")))?;
    let Some(arr) = json.as_array() else {
        return Err(LiveError::Parse("expected JSON array".to_string()));
    };

    let cleaned = extract_and_sanitize(arr, json_field, kind);
    if cleaned.is_empty() && !arr.is_empty() {
        log_warn!(
            "live fetch parsed 0 valid values for {label} from {} entries",
            arr.len()
        );
    }
    let elapsed = start.elapsed();
    log_debug!(
        "live fetch completed {label} host={} latency_ms={} items={} elapsed={:?}",
        sanitize_for_log(&config.host),
        elapsed.as_millis(),
        cleaned.len(),
        elapsed
    );
    Ok(cleaned)
}

/// Extract `json_field` from each array entry (bounded to
/// `2 * MAX_LIVE_ITEMS` raw values) and sanitize the results with `kind`'s
/// value filter.
pub(crate) fn extract_and_sanitize(
    arr: &[serde_json::Value],
    json_field: &str,
    kind: ResourceKind,
) -> Vec<String> {
    let mut raw_values: Vec<String> = Vec::new();
    for entry in arr {
        if let Some(obj) = entry.as_object()
            && let Some(val) = obj.get(json_field)
            && let Some(val_str) = val.as_str()
        {
            raw_values.push(val_str.to_string());
        }
        if raw_values.len() >= MAX_LIVE_ITEMS * 2 {
            break;
        }
    }
    sanitize_resource_values(raw_values, kind)
}

/// Fetch live data for a specific resource kind from the RouterOS REST API.
///
/// Thin wrapper over `fetch_live_resource`: builds the resource-specific URL
/// (which runs the shared host/port/scheme validation via `build_base_url_with_allow`)
/// and selects the resource's JSON field and value filter.
pub fn fetch_resource(
    config: &LiveConfig,
    resource: ResourceKind,
) -> Result<Vec<String>, LiveError> {
    if !config.is_active() {
        return Err(LiveError::Disabled);
    }
    let url = build_rest_url(config, resource)?;
    log_debug!(
        "live fetch_resource kind={:?} url={} user={} timeout={}s ssl_verify={} ssl_verify_effective={}",
        resource,
        sanitize_for_log(&url),
        sanitize_for_log(&config.user),
        config.timeout_secs,
        config.ssl_verify,
        config.ssl_verify_effective()
    );
    fetch_live_resource(
        config,
        &url,
        resource.json_field(),
        resource,
        &format!("{resource:?}"),
    )
}

/// Fetch live data for a custom resource (user-defined via
/// `RSC_LS_LIVE_RESOURCES`).
///
/// Thin wrapper over `fetch_live_resource`; custom values use the generic
/// identifier filter (same as interfaces).
pub fn fetch_custom_resource(
    config: &LiveConfig,
    custom: &CustomResource,
) -> Result<Vec<String>, LiveError> {
    if !config.is_active() {
        return Err(LiveError::Disabled);
    }
    let url = build_custom_rest_url(config, custom)?;
    log_debug!(
        "live fetch_custom kind={} url={} user={} timeout={}s",
        sanitize_for_log(&custom.property),
        sanitize_for_log(&url),
        sanitize_for_log(&config.user),
        config.timeout_secs
    );
    fetch_live_resource(
        config,
        &url,
        &custom.field,
        ResourceKind::Interfaces,
        &custom.property,
    )
}

/// Fetch interface names from the RouterOS REST API (wrapper for backwards compatibility).
#[cfg(test)]
pub fn fetch_interfaces(config: &LiveConfig) -> Result<Vec<String>, LiveError> {
    fetch_resource(config, ResourceKind::Interfaces)
}
