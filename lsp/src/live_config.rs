// ── Live configuration ───────────────────────────────────────────────────
//
// `LiveConfig` + env/settings/scheme/shim parsing and custom resources.
//
// Split from `live.rs`; re-exported there, `crate::live::…` paths unchanged.

use crate::caps::{
    LIVE_CUSTOM_RESOURCES_MAX, LIVE_MAX_HOSTS, LIVE_TIMEOUT_SECS, MAX_LIVE_VALUE_LEN,
};
use crate::live_cache::{ResourceKind, live_resource_for_menu_property};
use crate::live_net::{is_ssrf_denied_host, validate_host, validate_host_with_allow};
use crate::logging::{log_debug, log_info, log_warn, sanitize_for_log};
#[cfg(test)]
use std::sync::{Mutex, OnceLock};

// ── CustomResource ───────────────────────────────────────────────────────

/// User-defined live resource mapping via `RSC_LS_LIVE_RESOURCES`.
///
/// JSON shape: `{ "property": "packet-mark", "path": "/rest/ip/firewall/mangle", "field":
/// "new-packet-mark" }`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomResource {
    /// Property name that triggers this resource (e.g. "packet-mark").
    pub property: String,
    /// REST path on the device (e.g. "/rest/ip/firewall/mangle").
    pub path: String,
    /// JSON field to extract from each array entry (e.g. "new-packet-mark").
    pub field: String,
}

// ── LiveConfig ───────────────────────────────────────────────────────────

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
    /// All hosts when `MIKROTIK_HOST` is comma-separated (first is primary). Capped to
    /// `LIVE_MAX_HOSTS`.
    /// Multi-host is validated but only the primary host is currently fetched; additional hosts
    /// retained for future use.
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
    /// Default deny (false) — when false, `127.0.0.0/8`, `::1`, `10/8`, `192.168/16` etc are
    /// rejected via `is_loopback_or_private`.
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
/// `host`/`user` (F2), `port`, `ssl_verify=false`, `force_http=true`,
/// `allow_loopback=true`, `custom_resources`, and `ca_file` from settings
/// are ignored (env values always win).
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
    /// Transport-security keys (`host`, `user`, `port`, `ssl_verify=false`,
    /// `force_http=true`, `allow_loopback=true`, `custom_resources`,
    /// `ca_file`) are privileged: they are ignored from workspace settings
    /// unless `RSC_LS_ALLOW_SETTINGS_TRANSPORT=1` is set. Env values always
    /// win.
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
            // F2: the port selects the transport endpoint credentials are
            // sent to, so a settings change is privileged like host/user.
            // Default deny unless RSC_LS_ALLOW_SETTINGS_TRANSPORT=1; env
            // values always win when the opt-in is absent.
            if port_val != cfg.port && !allow_transport {
                log_warn!(
                    "live settings port ignored (was {} now {}): set RSC_LS_ALLOW_SETTINGS_TRANSPORT=1 to allow workspace transport overrides",
                    cfg.port,
                    port_val
                );
            } else {
                cfg.port = port_val;
            }
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
        // Fallback to custom resources: if property matches a custom mapping, treat as
        // interface-like.
        // We map custom to the closest built-in kind for now, or return Interfaces as generic.
        let prop_low = property.to_ascii_lowercase();
        for cr in &self.custom_resources {
            if cr.property.eq_ignore_ascii_case(&prop_low)
                || cr.property.eq_ignore_ascii_case(property)
            {
                // Custom resource matched — we still need a ResourceKind to drive cache key.
                // For now, return Interfaces as a generic live kind; future: use custom path/field
                // directly.
                // Better: return a dedicated handling via custom fetch; but for completion we can
                // treat as live.
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
    // `get(..7)` yields `None` when byte 7 is not a char boundary, so a
    // multi-byte character at the prefix position is rejected fail-closed
    // instead of panicking on the slice.
    if hex
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("sha256:"))
    {
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
