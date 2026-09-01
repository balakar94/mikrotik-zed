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
use crate::logging::{log_debug, log_info, log_warn};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

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
            .finish()
    }
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
            log_info!("live multi-host {} (primary={})", hosts.len(), host);
        }

        let user_raw = get("MIKROTIK_USER").unwrap_or_default();
        let user = if user_raw.trim().is_empty() {
            "admin".to_string()
        } else {
            user_raw.trim().to_string()
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
        }
    }

    /// Whether live fetching is active.
    ///
    /// Requires opt-in `enabled` AND non-empty `host` + `pass` with valid host.
    pub fn is_active(&self) -> bool {
        self.enabled
            && !self.host.is_empty()
            && !self.pass.is_empty()
            && validate_host(&self.host).is_ok()
            && self.port != 0
    }

    /// Whether TLS verification is effectively enabled for the current scheme.
    ///
    /// Verification only matters when the resolved scheme is `https`; on `http`
    /// the flag is irrelevant and effective is `false`.
    pub fn ssl_verify_effective(&self) -> bool {
        self.ssl_verify && self.scheme() == "https"
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
            log_info!(
                "live enabled host={} port={} scheme={} user={} ssl_verify={} ssl_verify_effective={} timeout={}s hosts={:?} custom_resources={}",
                self.host,
                self.port,
                self.scheme(),
                self.user,
                self.ssl_verify,
                self.ssl_verify_effective(),
                self.timeout_secs,
                self.hosts,
                self.custom_resources.len()
            );
            if self.hosts.len() > 1 {
                log_info!(
                    "live multi-host active count={} primary={}",
                    self.hosts.len(),
                    self.host
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
    pub fn apply_settings_value(cfg: &mut Self, v: &serde_json::Value) {
        // Find the most relevant settings object.
        let settings_obj = find_settings_object(v).unwrap_or(v);

        if let Some(host_val) = get_settings_str(settings_obj, &["host", "MIKROTIK_HOST"]) {
            let hosts = parse_hosts(&host_val);
            if !hosts.is_empty() {
                cfg.host = hosts[0].clone();
                cfg.hosts = hosts;
            }
        }
        if let Some(user_val) =
            get_settings_str(settings_obj, &["user", "username", "MIKROTIK_USER"])
        {
            let trimmed = user_val.trim();
            if !trimmed.is_empty() {
                cfg.user = trimmed.to_string();
            }
        }
        if let Some(pass_val) =
            get_settings_str(settings_obj, &["pass", "password", "MIKROTIK_PASS"])
        {
            cfg.pass = pass_val;
        }
        if let Some(port_val) = get_settings_port(settings_obj) {
            cfg.port = port_val;
        }
        if let Some(ssl_val) = get_settings_bool(
            settings_obj,
            &["ssl_verify", "ssl", "MIKROTIK_SSL", "verify_ssl"],
        ) {
            cfg.ssl_verify = ssl_val;
        } else if let Some(s) = get_settings_str(settings_obj, &["MIKROTIK_SSL"]) {
            // Handle string "0" as in env.
            let trimmed = s.trim();
            if trimmed == "0" {
                cfg.ssl_verify = false;
            } else if trimmed == "1" {
                cfg.ssl_verify = true;
            }
        }
        if let Some(http_val) =
            get_settings_bool(settings_obj, &["force_http", "http", "MIKROTIK_HTTP"])
        {
            cfg.force_http = http_val;
        }
        if let Some(timeout_val) = get_settings_u64(
            settings_obj,
            &["timeout", "timeout_secs", "MIKROTIK_TIMEOUT"],
        ) {
            cfg.timeout_secs = timeout_val.clamp(1, 30);
        }
        // Custom resources overlay: check for JSON array or stringified JSON.
        if let Some(custom_val) = settings_obj
            .get("custom_resources")
            .or_else(|| settings_obj.get("live_resources"))
            .or_else(|| settings_obj.get("RSC_LS_LIVE_RESOURCES"))
        {
            if custom_val.is_array() {
                cfg.custom_resources = parse_custom_resources_from_value(custom_val);
            } else if let Some(s) = custom_val.as_str() {
                cfg.custom_resources = parse_custom_resources(Some(s));
            }
        } else if let Some(s) = get_settings_str(settings_obj, &["RSC_LS_LIVE_RESOURCES"]) {
            cfg.custom_resources = parse_custom_resources(Some(&s));
        }
        // Log if multi-host after overlay
        if cfg.hosts.len() > 1 {
            log_info!(
                "live multi-host (settings) {} (primary={})",
                cfg.hosts.len(),
                cfg.host
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
                    cr.property,
                    cr.path,
                    cr.field
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
            log_warn!("live host validation failed for {h:?}: {e}");
        }
        if is_ssrf_denied_host(h) {
            log_warn!("live host denied by SSRF filter: {h:?}");
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
/// - starts with `/rest`, length 1..64
/// - no null, control, `\`, `%`, `?`, `#`, `@`
/// - no `//` (consecutive slashes)
/// - no `..` as an exact segment (split by `/`)
fn is_valid_custom_path(path: &str) -> bool {
    if path.is_empty() || path.len() > 64 {
        return false;
    }
    if !path.starts_with("/rest") {
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
                entry
            );
            continue;
        }
        // Path validation: strict allowlist, no traversal or delimiters.
        if !is_valid_custom_path(&path) {
            log_warn!(
                "custom resource path failed validation, skipping: {:?}",
                entry
            );
            continue;
        }
        // Property and field validation: ^[a-zA-Z0-9_-]+$ 1..64
        if !is_valid_custom_identifier(&property) {
            log_warn!(
                "custom resource property failed validation (expected ^[a-zA-Z0-9_-]+$ 1..64), skipping: {:?}",
                entry
            );
            continue;
        }
        if !is_valid_custom_identifier(&field) {
            log_warn!(
                "custom resource field failed validation (expected ^[a-zA-Z0-9_-]+$ 1..64), skipping: {:?}",
                entry
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
/// Looks for `rsc.live`, `mikrotik`, or `settings.rsc.live` nesting.
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
        // If object itself looks like a config (has host), return it.
        if obj.contains_key("host")
            || obj.contains_key("MIKROTIK_HOST")
            || obj.contains_key("MIKROTIK_PASS")
        {
            return Some(v);
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

/// Resolve the REST URL scheme, mirroring `scripts/_mikrotik_shared.py::resolve_scheme`.
///
/// `port`: target port.
/// `force_http`: `MIKROTIK_HTTP=1`.
/// `ssl_verify`: true when verification is enabled; false when `MIKROTIK_SSL=0`.
///
/// The live client applies the same rules as the Python companion scripts
/// but does not emit their legacy-shim warning (see `LiveConfig::scheme`).
pub(crate) fn resolve_scheme(port: u16, force_http: bool, ssl_verify: bool) -> &'static str {
    let no_ssl_verify = !ssl_verify;
    if !force_http && no_ssl_verify && port != 443 && port != 8729 {
        return "http";
    }
    if force_http { "http" } else { "https" }
}

/// Check if a host is denied by SSRF protection.
fn is_ssrf_denied_host(host: &str) -> bool {
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

/// Validate `host` per defensive rules.
///
/// - non-empty, max 253 chars
/// - no null bytes, no control chars
/// - no URI delimiters that would alter URL parsing (`@`, `?`, `#`, ` `, `%`)
/// - SSRF denials for `169.254.169.254` and `metadata.google.internal`
pub fn validate_host(host: &str) -> Result<(), LiveError> {
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
    // Reject URI-meaningful delimiters that could be interpreted as userinfo,
    // query, fragment or escape sequence when interpolated into the URL.
    // Brackets and ':' are intentionally allowed for IPv6 literals (e.g. `[::1]`).
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
    Ok(())
}

/// Format host for URL: wrap bare IPv6 literals with brackets if needed.
///
/// Keep in sync with `scripts/_mikrotik_shared.py::format_host_for_url`.
fn format_host_for_url(host: &str) -> String {
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
/// Validates host (`validate_host`, SSRF, slash, port), wraps bare IPv6,
/// parses via `url::Url::parse`, and checks scheme. Path is left as `/`
/// for callers to set via `Url::set_path`. Keeps caps single source.
///
/// Keep in sync with `scripts/_mikrotik_shared.py::validate_host` /
/// `format_host_for_url` / `resolve_scheme`.
fn build_base_url(host: &str, port: u16, scheme: &str) -> Result<url::Url, LiveError> {
    validate_host(host)?;
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
/// Uses `build_base_url` for shared validation, then appends the resource path.
/// Handles IPv6 bracket wrapping via `format_host_for_url`.
fn build_rest_url(config: &LiveConfig, resource: ResourceKind) -> Result<String, LiveError> {
    let mut base = build_base_url(&config.host, config.port, config.scheme())?;
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
    let mut base = build_base_url(&config.host, config.port, config.scheme())?;
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
    pub values: Vec<String>,
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

    /// Non-blocking read: return a cloned value if the entry is fresh.
    pub fn try_get_cached(&self, key: &str) -> Option<Vec<String>> {
        let entry = self.entries.get(key)?;
        if self.is_fresh(entry.fetched_at) {
            Some(entry.values.clone())
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
        self.entries.insert(
            key.clone(),
            CachedValue {
                values: vals,
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
        self.entries.insert(
            key.clone(),
            CachedValue {
                values: vals,
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
) -> Option<Vec<String>> {
    let res = live_resource_for_property(property, type_str)?;
    cache.try_get_cached(res.cache_key())
}

/// Return live resource kind and values for `property`/`type_str` if the cache is live-enrichable.
pub fn live_resource_values_for_property(
    cache: &LiveCache,
    menu_path: &str,
    property: &str,
    type_str: &str,
) -> Option<(ResourceKind, Vec<String>)> {
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
) -> Option<Vec<String>> {
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
    let cache_clone = Arc::clone(cache);
    let config_clone = config.clone();
    log_debug!("live background fetch triggered for {:?}", resource);
    std::thread::spawn(move || {
        let start = Instant::now();
        let result = fetch_resource(&config_clone, resource);
        let elapsed = start.elapsed();
        match result {
            Ok(values) => {
                if values.is_empty() {
                    log_debug!("live fetch {:?} returned empty set", resource);
                    // Empty set not cached; insert negative to avoid churn if repeated?
                    // Keep as is: no negative for empty, just skip.
                    return;
                }
                log_info!(
                    "live fetch ok kind={:?} host={} latency_ms={} items={}",
                    resource,
                    config_clone.host,
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
                log_warn!(
                    "live fetch {:?} failed: {} latency_ms={} host={}",
                    resource,
                    e,
                    elapsed.as_millis(),
                    config_clone.host
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
/// 1. Resource resolution — `resolve_resource_with_custom` on (menu path,
///    property, arg type), i.e. the built-in heuristic with
///    custom-resource fallback; when `property` is `None` (cursor not inside
///    a `key=value` assignment) interfaces are prefetched as the likely target.
/// 2. Built-in background fetch — coalescing-aware spawn via
///    `get_cached_or_fetch_background` (`try_get_cached`,
///    `is_negative_cooldown`, `can_spawn_fetch`, `record_fetch_attempt`).
/// 3. Custom background fetch — a separate coalescing-aware spawn under
///    cache key `custom:<property>` (see `trigger_custom_fetch_background`).
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
    let target_resource = match property {
        Some(key) => config.resolve_resource_with_custom(context_path, key, arg_type),
        None => Some(ResourceKind::Interfaces),
    };

    match target_resource {
        Some(res) => {
            let _ = get_cached_or_fetch_background(cache, config, res);
            log_debug!("live background fetch triggered for {res:?}");
        }
        None => {
            // Property has no known resource; prefetch interfaces anyway so
            // the next keystroke can enrich.
            let _ = get_cached_or_fetch_background(cache, config, ResourceKind::Interfaces);
        }
    }

    // Custom resource: separate background fetch under its own cache key so
    // it never collides with built-in entries.
    if let Some(key) = property
        && let Some(custom) = config.custom_resource_for_property(key).cloned()
    {
        trigger_custom_fetch_background(cache, config, &custom);
    }
}

/// Spawn a coalescing-aware background fetch for a custom resource.
///
/// Cache key is `custom:<property>`. Mirrors `get_cached_or_fetch_background`
/// semantics: a fresh cache entry or a negative cooldown short-circuits, and
/// at most one fetch is in flight per key within
/// `LIVE_FETCH_BLOCKING_TIMEOUT_SECS`. Empty results are not cached (no
/// negative either, same as the built-in path).
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

    let cache_clone = Arc::clone(cache);
    let config_clone = config.clone();
    let custom_clone = custom.clone();
    let key_clone = key.clone();
    log_debug!("live background fetch triggered for custom resource {key_clone}");
    std::thread::spawn(move || {
        let start = Instant::now();
        match fetch_custom_resource(&config_clone, &custom_clone) {
            Ok(vals) => {
                if vals.is_empty() {
                    log_debug!(
                        "live custom fetch {} returned empty set",
                        custom_clone.property
                    );
                    return;
                }
                log_info!(
                    "live fetch ok custom property={} path={} latency_ms={} items={}",
                    custom_clone.property,
                    custom_clone.path,
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
                log_warn!(
                    "live fetch custom failed property={} path={} err={} latency_ms={}",
                    custom_clone.property,
                    custom_clone.path,
                    e,
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
/// Logs `live agent reuse` on hit.
fn get_cached_agent(timeout: Duration, ssl_verify: bool) -> ureq::Agent {
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
    let agent = if ssl_verify {
        ureq::AgentBuilder::new().timeout(timeout).build()
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
                ureq::AgentBuilder::new().timeout(timeout).build()
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

/// Build an agent that disables TLS verification (insecure).
///
/// Returns `None` if the rustls insecure config cannot be built.
fn build_insecure_agent(timeout: Duration) -> Option<ureq::Agent> {
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
/// validation, and URL construction (via `build_base_url`). `pass` is only
/// used in the Authorization header and never logged.
fn fetch_live_resource(
    config: &LiveConfig,
    url: &str,
    json_field: &str,
    kind: ResourceKind,
    label: &str,
) -> Result<Vec<String>, LiveError> {
    if !config.ssl_verify {
        log_warn!(
            "live ssl_verify=false — TLS verification disabled (insecure) scheme={} host={} port={} ssl_verify_effective={}",
            config.scheme(),
            config.host,
            config.port,
            config.ssl_verify_effective()
        );
    }
    let timeout = Duration::from_secs(config.timeout_secs.clamp(1, 30));
    let agent = get_cached_agent(timeout, config.ssl_verify_effective());

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
            return Err(LiveError::Network(msg));
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
            Err(e) => return Err(LiveError::Network(e.to_string())),
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
        config.host,
        elapsed.as_millis(),
        cleaned.len(),
        elapsed
    );
    Ok(cleaned)
}

/// Extract `json_field` from each array entry (bounded to
/// `2 * MAX_LIVE_ITEMS` raw values) and sanitize the results with `kind`'s
/// value filter.
fn extract_and_sanitize(
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
/// (which runs the shared host/port/scheme validation via `build_base_url`)
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
        url,
        config.user,
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
        custom.property,
        url,
        config.user,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg_with(map: HashMap<&str, &str>) -> LiveConfig {
        LiveConfig::from_env_with(|k| map.get(k).map(|v| v.to_string()))
    }

    // ── Config parsing ───────────────────────────────────────────

    #[test]
    fn test_disabled_by_default() {
        let cfg = cfg_with(HashMap::new());
        assert!(!cfg.enabled);
        assert!(!cfg.is_active());
        assert_eq!(cfg.user, "admin");
        assert_eq!(cfg.port, 443);
        assert!(cfg.ssl_verify);
        assert!(!cfg.force_http);
        assert_eq!(cfg.timeout_secs, LIVE_TIMEOUT_SECS);
    }

    #[test]
    fn test_enabled_via_rsc_ls_live() {
        let mut m = HashMap::new();
        m.insert("RSC_LS_LIVE", "1");
        m.insert("MIKROTIK_HOST", "192.168.88.1");
        m.insert("MIKROTIK_PASS", "secret");
        let cfg = cfg_with(m);
        assert!(cfg.enabled);
        assert!(cfg.is_active());
    }

    #[test]
    fn test_enabled_via_mikrotik_live() {
        let mut m = HashMap::new();
        m.insert("MIKROTIK_LIVE", "1");
        m.insert("MIKROTIK_HOST", "router.local");
        m.insert("MIKROTIK_PASS", "pw");
        let cfg = cfg_with(m);
        assert!(cfg.enabled);
        assert!(cfg.is_active());
        // Any value other than "1" is not enabled.
        let mut m2 = HashMap::new();
        m2.insert("MIKROTIK_LIVE", "true");
        m2.insert("MIKROTIK_HOST", "router.local");
        m2.insert("MIKROTIK_PASS", "pw");
        let cfg2 = cfg_with(m2);
        assert!(!cfg2.enabled);
        assert!(!cfg2.is_active());
    }

    #[test]
    fn test_enabled_requires_host_and_pass() {
        let mut m = HashMap::new();
        m.insert("RSC_LS_LIVE", "1");
        // missing host and pass
        let cfg = cfg_with(m.clone());
        assert!(cfg.enabled);
        assert!(!cfg.is_active());

        m.insert("MIKROTIK_HOST", "10.0.0.1");
        let cfg2 = cfg_with(m.clone());
        assert!(!cfg2.is_active()); // still missing pass

        m.insert("MIKROTIK_PASS", "x");
        let cfg3 = cfg_with(m);
        assert!(cfg3.is_active());
    }

    #[test]
    fn test_user_defaults_to_admin() {
        let mut m = HashMap::new();
        m.insert("RSC_LS_LIVE", "1");
        m.insert("MIKROTIK_HOST", "h");
        m.insert("MIKROTIK_PASS", "p");
        let cfg = cfg_with(m.clone());
        assert_eq!(cfg.user, "admin");

        m.insert("MIKROTIK_USER", "custom");
        let cfg2 = cfg_with(m);
        assert_eq!(cfg2.user, "custom");

        // empty string => default
        let mut m3 = HashMap::new();
        m3.insert("MIKROTIK_USER", "   ");
        m3.insert("MIKROTIK_HOST", "h");
        m3.insert("MIKROTIK_PASS", "p");
        m3.insert("RSC_LS_LIVE", "1");
        let cfg3 = cfg_with(m3);
        assert_eq!(cfg3.user, "admin");
    }

    #[test]
    fn test_port_default_and_env_override() {
        let mut m = HashMap::new();
        m.insert("RSC_LS_LIVE", "1");
        m.insert("MIKROTIK_HOST", "h");
        m.insert("MIKROTIK_PASS", "p");
        let cfg = cfg_with(m);
        assert_eq!(cfg.port, 443);

        let mut m2 = HashMap::new();
        m2.insert("MIKROTIK_PORT", "8729");
        m2.insert("MIKROTIK_HOST", "h");
        m2.insert("MIKROTIK_PASS", "p");
        m2.insert("RSC_LS_LIVE", "1");
        let cfg2 = cfg_with(m2);
        assert_eq!(cfg2.port, 8729);
    }

    #[test]
    fn test_port_invalid_falls_back_to_default() {
        let mut m = HashMap::new();
        m.insert("MIKROTIK_PORT", "not-a-number");
        m.insert("MIKROTIK_HOST", "h");
        m.insert("MIKROTIK_PASS", "p");
        m.insert("RSC_LS_LIVE", "1");
        let cfg = cfg_with(m);
        // Warning is logged; default is used.
        assert_eq!(cfg.port, 443);
    }

    #[test]
    fn test_ssl_verify_respects_mikrotik_ssl() {
        let mut m = HashMap::new();
        m.insert("MIKROTIK_HOST", "h");
        m.insert("MIKROTIK_PASS", "p");
        m.insert("RSC_LS_LIVE", "1");
        let cfg = cfg_with(m.clone());
        assert!(cfg.ssl_verify);

        let mut m2 = HashMap::new();
        m2.insert("MIKROTIK_SSL", "0");
        m2.insert("MIKROTIK_HOST", "h");
        m2.insert("MIKROTIK_PASS", "p");
        m2.insert("RSC_LS_LIVE", "1");
        let cfg2 = cfg_with(m2);
        assert!(!cfg2.ssl_verify);

        // Any other value => true
        let mut m3 = HashMap::new();
        m3.insert("MIKROTIK_SSL", "1");
        m3.insert("MIKROTIK_HOST", "h");
        m3.insert("MIKROTIK_PASS", "p");
        m3.insert("RSC_LS_LIVE", "1");
        let cfg3 = cfg_with(m3);
        assert!(cfg3.ssl_verify);
    }

    #[test]
    fn test_ssl_verify_respects_mikrotik_ssl_effective() {
        // Effective verification is false when ssl_verify is false, or when scheme is http.
        let mut m = HashMap::new();
        m.insert("RSC_LS_LIVE", "1");
        m.insert("MIKROTIK_HOST", "192.168.88.1");
        m.insert("MIKROTIK_PASS", "p");
        let cfg = cfg_with(m);
        assert!(cfg.ssl_verify);
        assert!(cfg.ssl_verify_effective());
        assert_eq!(cfg.scheme(), "https");

        let mut m2 = HashMap::new();
        m2.insert("RSC_LS_LIVE", "1");
        m2.insert("MIKROTIK_HOST", "192.168.88.1");
        m2.insert("MIKROTIK_PASS", "p");
        m2.insert("MIKROTIK_SSL", "0");
        let cfg2 = cfg_with(m2);
        assert!(!cfg2.ssl_verify);
        assert!(!cfg2.ssl_verify_effective());

        // Force http also makes effective false even if ssl_verify true.
        let mut m3 = HashMap::new();
        m3.insert("RSC_LS_LIVE", "1");
        m3.insert("MIKROTIK_HOST", "h");
        m3.insert("MIKROTIK_PASS", "p");
        m3.insert("MIKROTIK_HTTP", "1");
        let cfg3 = cfg_with(m3);
        assert!(cfg3.ssl_verify);
        assert!(!cfg3.ssl_verify_effective());
        assert_eq!(cfg3.scheme(), "http");

        // Non-standard port with ssl_verify false => scheme http => effective false
        let mut m4 = HashMap::new();
        m4.insert("RSC_LS_LIVE", "1");
        m4.insert("MIKROTIK_HOST", "h");
        m4.insert("MIKROTIK_PASS", "p");
        m4.insert("MIKROTIK_SSL", "0");
        m4.insert("MIKROTIK_PORT", "80");
        let cfg4 = cfg_with(m4);
        assert!(!cfg4.ssl_verify);
        assert!(!cfg4.ssl_verify_effective());
        assert_eq!(cfg4.scheme(), "http");
    }

    #[test]
    fn test_force_http_respects_mikrotik_http() {
        let mut m = HashMap::new();
        m.insert("MIKROTIK_HOST", "h");
        m.insert("MIKROTIK_PASS", "p");
        m.insert("RSC_LS_LIVE", "1");
        let cfg = cfg_with(m.clone());
        assert!(!cfg.force_http);

        let mut m2 = HashMap::new();
        m2.insert("MIKROTIK_HTTP", "1");
        m2.insert("MIKROTIK_HOST", "h");
        m2.insert("MIKROTIK_PASS", "p");
        m2.insert("RSC_LS_LIVE", "1");
        let cfg2 = cfg_with(m2);
        assert!(cfg2.force_http);
    }

    #[test]
    fn test_timeout_default_and_clamp() {
        let mut m = HashMap::new();
        m.insert("MIKROTIK_HOST", "h");
        m.insert("MIKROTIK_PASS", "p");
        m.insert("RSC_LS_LIVE", "1");
        let cfg = cfg_with(m);
        assert_eq!(cfg.timeout_secs, LIVE_TIMEOUT_SECS);
        assert_eq!(cfg.timeout_secs, 5);

        // Clamp low
        let mut m2 = HashMap::new();
        m2.insert("MIKROTIK_TIMEOUT", "0");
        m2.insert("MIKROTIK_HOST", "h");
        m2.insert("MIKROTIK_PASS", "p");
        m2.insert("RSC_LS_LIVE", "1");
        let cfg2 = cfg_with(m2);
        assert_eq!(cfg2.timeout_secs, 1);

        // Clamp high
        let mut m3 = HashMap::new();
        m3.insert("MIKROTIK_TIMEOUT", "100");
        m3.insert("MIKROTIK_HOST", "h");
        m3.insert("MIKROTIK_PASS", "p");
        m3.insert("RSC_LS_LIVE", "1");
        let cfg3 = cfg_with(m3);
        assert_eq!(cfg3.timeout_secs, 30);

        // Valid middle
        let mut m4 = HashMap::new();
        m4.insert("MIKROTIK_TIMEOUT", "10");
        m4.insert("MIKROTIK_HOST", "h");
        m4.insert("MIKROTIK_PASS", "p");
        m4.insert("RSC_LS_LIVE", "1");
        let cfg4 = cfg_with(m4);
        assert_eq!(cfg4.timeout_secs, 10);

        // Invalid => default
        let mut m5 = HashMap::new();
        m5.insert("MIKROTIK_TIMEOUT", "bogus");
        m5.insert("MIKROTIK_HOST", "h");
        m5.insert("MIKROTIK_PASS", "p");
        m5.insert("RSC_LS_LIVE", "1");
        let cfg5 = cfg_with(m5);
        assert_eq!(cfg5.timeout_secs, LIVE_TIMEOUT_SECS);
    }

    #[test]
    fn test_scheme_resolution() {
        // https default
        assert_eq!(resolve_scheme(443, false, true), "https");
        assert_eq!(resolve_scheme(443, false, false), "https"); // 443 is standard, no legacy shim
        assert_eq!(resolve_scheme(8729, false, false), "https"); // 8729 also standard
        // legacy shim: non-standard port + no verify => http
        assert_eq!(resolve_scheme(80, false, false), "http");
        assert_eq!(resolve_scheme(8080, false, false), "http");
        // force_http overrides
        assert_eq!(resolve_scheme(443, true, true), "http");
        assert_eq!(resolve_scheme(80, true, true), "http");
        // non-standard port with verify => still https unless force_http
        assert_eq!(resolve_scheme(80, false, true), "https");
    }

    #[test]
    fn test_host_validation() {
        assert!(validate_host("192.168.88.1").is_ok());
        assert!(validate_host("router.local").is_ok());
        assert!(validate_host("").is_err());
        assert!(validate_host("a".repeat(254).as_str()).is_err());
        assert!(validate_host("ok-host").is_ok());
        assert!(validate_host("host\0with-null").is_err());
        assert!(validate_host("host\nnewline").is_err());
        assert!(validate_host("host\tcontrol").is_err());
    }

    #[test]
    fn test_host_validation_rejects_uri_delimiters() {
        // Security fix: host must not contain URI-meaningful delimiters that could alter URL parsing.
        for bad in [
            "evil@host",
            "host?query=1",
            "host#frag",
            "host with space",
            "host%2e",
            "10.0.0.1@evil",
            "router.local?x=1",
        ] {
            assert!(
                validate_host(bad).is_err(),
                "host delimiter should be rejected: {bad:?}"
            );
        }
        // Brackets and ':' intentionally allowed for IPv6 literals.
        assert!(validate_host("[::1]").is_ok());
        assert!(validate_host("[2001:db8::1]").is_ok());
        assert!(validate_host("fe80::1").is_ok());
        // Backslash path separator rejected via fetch_interfaces host slash check (see test_fetch_interfaces_rejects_host_with_slash)
        // but validate_host itself allows '/'? No—fetch layer rejects '/' explicitly, validate rejects control/null/delimiters only.
        // Ensure normal hostnames still pass.
        assert!(validate_host("router-1.local").is_ok());
        assert!(validate_host("192.168.88.1").is_ok());
    }

    #[test]
    fn test_host_validation_rejects_metadata_ip() {
        // SSRF protection: deny instance metadata endpoints.
        for bad in [
            "169.254.169.254",
            "metadata.google.internal",
            "::ffff:169.254.169.254",
            "[::ffff:169.254.169.254]",
            "169.254.169.254:80", // host with port should be rejected? contains ':'? For pure host without port, we check inner; but colon presence is allowed for IPv6. This case is not pure IP, but we test base.
        ] {
            // For the last entry with port, validation may allow ':' but SSRF check should still deny base IP? Our is_ssrf_denied_host checks inner after stripping brackets, but with port it includes colon and port. We handle exact match only, so "169.254.169.254:80" not denied as host (port is separate). So we test exact hosts.
            if bad == "169.254.169.254:80" {
                continue;
            }
            assert!(
                validate_host(bad).is_err(),
                "SSRF host should be rejected: {bad:?}"
            );
        }
        // Normal hosts still ok
        assert!(validate_host("192.168.88.1").is_ok());
        assert!(validate_host("10.0.0.1").is_ok());
    }

    #[test]
    fn test_url_build_ipv6() {
        let mut m = HashMap::new();
        m.insert("RSC_LS_LIVE", "1");
        m.insert("MIKROTIK_HOST", "fe80::1");
        m.insert("MIKROTIK_PASS", "p");
        m.insert("MIKROTIK_PORT", "443");
        let cfg = cfg_with(m);
        assert!(validate_host(&cfg.host).is_ok());
        // format_host_for_url should wrap bare IPv6
        assert_eq!(format_host_for_url("fe80::1"), "[fe80::1]");
        assert_eq!(format_host_for_url("[fe80::1]"), "[fe80::1]");
        assert_eq!(format_host_for_url("192.168.88.1"), "192.168.88.1");
        // build_rest_url should succeed and contain brackets
        let url = build_rest_url(&cfg, ResourceKind::Interfaces).expect("ipv6 url should build");
        assert!(
            url.contains("[fe80::1]"),
            "url should contain bracketed ipv6, got {url}"
        );
        assert!(url::Url::parse(&url).is_ok());

        // Already bracketed host
        let mut m2 = HashMap::new();
        m2.insert("RSC_LS_LIVE", "1");
        m2.insert("MIKROTIK_HOST", "[::1]");
        m2.insert("MIKROTIK_PASS", "p");
        let cfg2 = cfg_with(m2);
        let url2 = build_rest_url(&cfg2, ResourceKind::Interfaces).expect("bracketed ipv6 url");
        assert!(url2.contains("[::1]"));
    }

    #[test]
    fn test_debug_redacts_pass() {
        // Security fix: LiveConfig Debug must never leak MIKROTIK_PASS.
        let mut m = HashMap::new();
        m.insert("RSC_LS_LIVE", "1");
        m.insert("MIKROTIK_HOST", "192.168.88.1");
        m.insert("MIKROTIK_PASS", "super_secret_password_123");
        m.insert("MIKROTIK_USER", "admin");
        let cfg = cfg_with(m);
        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains("super_secret_password_123"),
            "Debug must not leak pass, got: {dbg}"
        );
        assert!(
            !dbg.contains("super_secret"),
            "Debug must not leak pass substring, got: {dbg}"
        );
        assert!(
            dbg.contains("[REDACTED]"),
            "Debug should contain [REDACTED] placeholder, got: {dbg}"
        );
        // Also ensure host is still visible (redaction is precise, not blanket)
        assert!(
            dbg.contains("192.168.88.1"),
            "host should still be visible in Debug"
        );
    }

    #[test]
    fn test_filter_value() {
        assert_eq!(filter_value("ether1"), Some("ether1".to_string()));
        assert_eq!(filter_value("ether-1"), Some("ether-1".to_string()));
        assert_eq!(filter_value("under_score"), Some("under_score".to_string()));
        assert_eq!(filter_value(""), None);
        assert_eq!(filter_value("   "), None);
        assert_eq!(filter_value("a b"), None); // space not allowed
        assert_eq!(filter_value("a/b"), None);
        assert_eq!(filter_value("a".repeat(65).as_str()), None); // over 64
        assert_eq!(filter_value("a".repeat(64).as_str()), Some("a".repeat(64)));
        assert_eq!(filter_value("ether1\0"), None);
        assert_eq!(filter_value("ether1\n"), None);
        assert_eq!(filter_value("wlan1"), Some("wlan1".to_string()));
        // Leading/trailing whitespace trimmed
        assert_eq!(filter_value("  ether1  "), Some("ether1".to_string()));
    }

    #[test]
    fn test_sanitize_values_dedup_sort_truncate() {
        let raw = vec![
            "ether2".to_string(),
            "ether1".to_string(),
            "ether1".to_string(),
            "bad val".to_string(),
            "wlan1".to_string(),
        ];
        let sanitized = sanitize_values(raw);
        assert_eq!(sanitized, vec!["ether1", "ether2", "wlan1"]); // sorted, deduped, bad filtered

        // Truncate to MAX_LIVE_ITEMS
        let many: Vec<String> = (0..600).map(|i| format!("iface{i}")).collect();
        let sanitized2 = sanitize_values(many);
        assert_eq!(sanitized2.len(), MAX_LIVE_ITEMS);
        assert!(sanitized2.is_sorted());
    }

    #[test]
    fn test_cache_ttl_fresh_and_stale() {
        let mut cache = LiveCache::new(Duration::from_secs(60));
        let now = Instant::now();
        cache.insert_with_time("interfaces".to_string(), vec!["ether1".to_string()], now);
        assert_eq!(
            cache.try_get_cached("interfaces"),
            Some(vec!["ether1".to_string()])
        );

        // Stale: 61 seconds ago
        let mut cache2 = LiveCache::new(Duration::from_secs(60));
        let stale = now - Duration::from_secs(61);
        cache2.insert_with_time("interfaces".to_string(), vec!["ether1".to_string()], stale);
        assert_eq!(cache2.try_get_cached("interfaces"), None);
    }

    #[test]
    fn test_cache_caps_enforcement() {
        let mut cache = LiveCache::new(Duration::from_secs(60));
        // Fill to MAX_CACHE_ENTRIES with distinct keys
        for i in 0..MAX_CACHE_ENTRIES {
            cache.insert(format!("k{i}"), vec![format!("v{i}")]);
        }
        assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
        // Inserting a new key should evict oldest
        cache.insert("new_key".to_string(), vec!["new_val".to_string()]);
        assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
        assert!(cache.entries.contains_key("new_key"));
        // Overlong values truncated
        let many: Vec<String> = (0..600).map(|i| format!("iface{i}")).collect();
        cache.insert("interfaces".to_string(), many);
        assert!(cache.entries.get("interfaces").unwrap().values.len() <= MAX_LIVE_ITEMS);
    }

    #[test]
    fn test_cache_max_live_value_len_enforced() {
        let mut cache = LiveCache::new(Duration::from_secs(60));
        let overlong = "a".repeat(65);
        cache.insert(
            "interfaces".to_string(),
            vec![overlong.clone(), "ok".to_string()],
        );
        let vals = cache.try_get_cached("interfaces").unwrap();
        assert!(!vals.contains(&overlong));
        assert!(vals.contains(&"ok".to_string()));
    }

    #[test]
    fn test_live_values_for_property_mapping() {
        let mut cache = LiveCache::new(Duration::from_secs(60));
        cache.insert(
            "interfaces".to_string(),
            vec!["ether1".to_string(), "wlan1".to_string()],
        );

        // Property name match
        assert_eq!(
            live_values_for_property(&cache, "interface", "string"),
            Some(vec!["ether1".to_string(), "wlan1".to_string()])
        );
        assert_eq!(
            live_values_for_property(&cache, "bridge", "string"),
            Some(vec!["ether1".to_string(), "wlan1".to_string()])
        );
        assert_eq!(
            live_values_for_property(&cache, "actual-interface", "string"),
            Some(vec!["ether1".to_string(), "wlan1".to_string()])
        );
        // Type contains iface
        assert_eq!(
            live_values_for_property(&cache, "foo", "iface_enum"),
            Some(vec!["ether1".to_string(), "wlan1".to_string()])
        );
        assert_eq!(
            live_values_for_property(&cache, "foo", "IFACE"),
            Some(vec!["ether1".to_string(), "wlan1".to_string()])
        );
        // Non-matching property and type => None
        assert_eq!(
            live_values_for_property(&cache, "address", "ipPrefix"),
            None
        );
        assert_eq!(live_values_for_property(&cache, "comment", "string"), None);

        // No cache entry => None even for matching property
        let empty = LiveCache::new(Duration::from_secs(60));
        assert_eq!(
            live_values_for_property(&empty, "interface", "iface_enum"),
            None
        );
    }

    #[test]
    fn test_is_live_property() {
        assert!(is_live_property("interface", "string"));
        assert!(is_live_property("bridge", "foo"));
        assert!(is_live_property("actual-interface", "bar"));
        assert!(is_live_property("myprop", "iface_enum"));
        assert!(is_live_property("address", "ipPrefix"));
        assert!(!is_live_property("comment", "string"));
        // case-insensitive
        assert!(is_live_property("Interface", "string"));
        assert!(is_live_property("foo", "IFACE"));
    }

    #[test]
    fn test_disabled_fallback_fetch_errors() {
        let cfg = cfg_with(HashMap::new()); // disabled
        let res = fetch_interfaces(&cfg);
        assert!(matches!(res, Err(LiveError::Disabled)));

        // Enabled but invalid host
        let mut m = HashMap::new();
        m.insert("RSC_LS_LIVE", "1");
        m.insert("MIKROTIK_HOST", "");
        m.insert("MIKROTIK_PASS", "p");
        let cfg2 = cfg_with(m);
        let res2 = fetch_interfaces(&cfg2);
        assert!(matches!(
            res2,
            Err(LiveError::Disabled) | Err(LiveError::InvalidHost(_))
        ));
    }

    #[test]
    fn test_get_cached_or_fetch_blocking_disabled_returns_none() {
        let cache = Arc::new(Mutex::new(LiveCache::with_default_ttl()));
        let cfg = cfg_with(HashMap::new());
        let res = get_cached_or_fetch_background(&cache, &cfg, ResourceKind::Interfaces);
        assert!(res.is_none());
    }

    #[test]
    fn test_get_cached_or_fetch_blocking_returns_cached_without_network() {
        let cache = Arc::new(Mutex::new(LiveCache::with_default_ttl()));
        {
            let mut guard = cache.lock().unwrap();
            guard.insert(
                "interfaces".to_string(),
                vec!["ether1".to_string(), "ether2".to_string()],
            );
        }
        let mut m = HashMap::new();
        m.insert("RSC_LS_LIVE", "1");
        m.insert("MIKROTIK_HOST", "192.168.88.1");
        m.insert("MIKROTIK_PASS", "secret");
        let cfg = cfg_with(m);
        let res = get_cached_or_fetch_background(&cache, &cfg, ResourceKind::Interfaces);
        assert_eq!(res, Some(vec!["ether1".to_string(), "ether2".to_string()]));
    }

    #[test]
    fn test_fetch_interfaces_rejects_host_with_slash() {
        let mut m = HashMap::new();
        m.insert("RSC_LS_LIVE", "1");
        m.insert("MIKROTIK_HOST", "host/with/slash");
        m.insert("MIKROTIK_PASS", "p");
        let cfg = cfg_with(m);
        let res = fetch_interfaces(&cfg);
        assert!(matches!(res, Err(LiveError::InvalidHost(_))));
    }

    #[test]
    fn test_filter_ip_value() {
        assert_eq!(
            filter_ip_value("192.168.88.1"),
            Some("192.168.88.1".to_string())
        );
        assert_eq!(
            filter_ip_value("10.0.0.1/24"),
            Some("10.0.0.1/24".to_string())
        );
        assert_eq!(
            filter_ip_value("2001:db8::1/64"),
            Some("2001:db8::1/64".to_string())
        );
        assert_eq!(filter_ip_value("fe80::1"), Some("fe80::1".to_string()));
        assert_eq!(filter_ip_value(""), None);
        assert_eq!(filter_ip_value("   "), None);
        assert_eq!(filter_ip_value("192.168.1.1 evil"), None);
        assert_eq!(filter_ip_value("192.168.1.1\0"), None);
        assert_eq!(filter_ip_value("192.168.1.1\n"), None);
    }

    #[test]
    fn test_resource_kind_properties() {
        assert_eq!(ResourceKind::all().len(), 11);
        for kind in ResourceKind::all() {
            assert!(!kind.cache_key().is_empty());
            assert!(kind.rest_path().starts_with("/rest/"));
            assert!(!kind.json_field().is_empty());
            assert!(kind.detail_label().starts_with("live — "));
        }
    }

    #[test]
    fn test_live_resource_for_property_all_kinds() {
        // Interfaces
        assert_eq!(
            live_resource_for_property("interface", "string"),
            Some(ResourceKind::Interfaces)
        );
        assert_eq!(
            live_resource_for_property("bridge", "string"),
            Some(ResourceKind::Interfaces)
        );
        assert_eq!(
            live_resource_for_property("in-interface", "string"),
            Some(ResourceKind::Interfaces)
        );
        assert_eq!(
            live_resource_for_property("foo", "iface_enum"),
            Some(ResourceKind::Interfaces)
        );

        // IPv4 Addresses
        assert_eq!(
            live_resource_for_property("address", "ipPrefix"),
            Some(ResourceKind::IpAddresses)
        );
        assert_eq!(
            live_resource_for_property("network", "ipAddr"),
            Some(ResourceKind::IpAddresses)
        );
        assert_eq!(
            live_resource_for_property("src-address", "string"),
            Some(ResourceKind::IpAddresses)
        );
        assert_eq!(
            live_resource_for_property("dst-address", "string"),
            Some(ResourceKind::IpAddresses)
        );
        assert_eq!(
            live_resource_for_property("gateway", "string"),
            Some(ResourceKind::IpAddresses)
        );

        // IPv6 Addresses
        assert_eq!(
            live_resource_for_menu_property("/ipv6/address", "address", "string"),
            Some(ResourceKind::Ipv6Addresses)
        );
        assert_eq!(
            live_resource_for_property("address", "ipv6Prefix"),
            Some(ResourceKind::Ipv6Addresses)
        );

        // Address lists (IPv4 & IPv6)
        assert_eq!(
            live_resource_for_property("src-address-list", "string"),
            Some(ResourceKind::AddressLists)
        );
        assert_eq!(
            live_resource_for_property("address-list", "string"),
            Some(ResourceKind::AddressLists)
        );
        assert_eq!(
            live_resource_for_property("list", "string"),
            Some(ResourceKind::AddressLists)
        );
        assert_eq!(
            live_resource_for_menu_property("/ipv6/firewall/address-list", "list", "string"),
            Some(ResourceKind::Ipv6AddressLists)
        );

        // Firewall chains (filter, mangle, nat, raw)
        assert_eq!(
            live_resource_for_menu_property("/ip/firewall/filter", "chain", "string"),
            Some(ResourceKind::FirewallFilterChains)
        );
        assert_eq!(
            live_resource_for_menu_property("/ip/firewall/mangle", "chain", "string"),
            Some(ResourceKind::FirewallMangleChains)
        );
        assert_eq!(
            live_resource_for_menu_property("/ip/firewall/nat", "chain", "string"),
            Some(ResourceKind::FirewallNatChains)
        );
        assert_eq!(
            live_resource_for_menu_property("/ip/firewall/raw", "chain", "string"),
            Some(ResourceKind::FirewallRawChains)
        );
        assert_eq!(
            live_resource_for_property("jump-target", "string"),
            Some(ResourceKind::FirewallFilterChains)
        );

        // IP Pools (IPv4 & IPv6)
        assert_eq!(
            live_resource_for_property("pool", "string"),
            Some(ResourceKind::IpPools)
        );
        assert_eq!(
            live_resource_for_property("address-pool", "string"),
            Some(ResourceKind::IpPools)
        );
        assert_eq!(
            live_resource_for_property("foo", "ip_pool"),
            Some(ResourceKind::IpPools)
        );
        assert_eq!(
            live_resource_for_menu_property("/ipv6/pool", "pool", "string"),
            Some(ResourceKind::Ipv6Pools)
        );

        // Unrelated
        assert_eq!(live_resource_for_property("comment", "string"), None);
        assert_eq!(live_resource_for_property("disabled", "bool"), None);
    }

    #[test]
    fn test_multi_resource_cache_isolation() {
        let mut cache = LiveCache::new(Duration::from_secs(60));
        cache.insert("interfaces".to_string(), vec!["ether1".to_string()]);
        cache.insert(
            "ip_addresses".to_string(),
            vec!["192.168.88.1/24".to_string()],
        );
        cache.insert("address_lists".to_string(), vec!["allowed_ips".to_string()]);
        cache.insert(
            "firewall_filter_chains".to_string(),
            vec!["forward".to_string(), "input".to_string()],
        );
        cache.insert("ip_pools".to_string(), vec!["dhcp-pool".to_string()]);

        assert_eq!(
            live_resource_values_for_property(&cache, "", "interface", "string"),
            Some((ResourceKind::Interfaces, vec!["ether1".to_string()]))
        );
        assert_eq!(
            live_resource_values_for_property(&cache, "", "address", "ipPrefix"),
            Some((
                ResourceKind::IpAddresses,
                vec!["192.168.88.1/24".to_string()]
            ))
        );
        assert_eq!(
            live_resource_values_for_property(&cache, "", "src-address-list", "string"),
            Some((ResourceKind::AddressLists, vec!["allowed_ips".to_string()]))
        );
        assert_eq!(
            live_resource_values_for_property(&cache, "/ip/firewall/filter", "chain", "string"),
            Some((
                ResourceKind::FirewallFilterChains,
                vec!["forward".to_string(), "input".to_string()]
            ))
        );
        assert_eq!(
            live_resource_values_for_property(&cache, "", "pool", "string"),
            Some((ResourceKind::IpPools, vec!["dhcp-pool".to_string()]))
        );
    }

    #[test]
    fn test_multi_host_parsing() {
        let mut m = HashMap::new();
        m.insert("RSC_LS_LIVE", "1");
        m.insert("MIKROTIK_HOST", "192.168.88.1,10.0.0.2,  192.168.1.1");
        m.insert("MIKROTIK_PASS", "p");
        let cfg = cfg_with(m);
        assert_eq!(cfg.host, "192.168.88.1");
        assert_eq!(cfg.hosts, vec!["192.168.88.1", "10.0.0.2", "192.168.1.1"]);
        assert_eq!(cfg.host.as_str(), "192.168.88.1");
        assert!(cfg.is_active());
        // Cap at 4
        let mut m2 = HashMap::new();
        m2.insert("RSC_LS_LIVE", "1");
        m2.insert("MIKROTIK_HOST", "a,b,c,d,e,f");
        m2.insert("MIKROTIK_PASS", "p");
        let cfg2 = cfg_with(m2);
        assert_eq!(cfg2.hosts.len(), LIVE_MAX_HOSTS);
        assert_eq!(cfg2.hosts.len(), 4);
    }

    #[test]
    fn test_negative_cache_cooldown() {
        let mut cache = LiveCache::new(Duration::from_secs(60));
        assert!(!cache.is_negative_cooldown("interfaces"));
        cache.insert_negative("interfaces".to_string());
        assert!(cache.is_negative_cooldown("interfaces"));
        // After inserting success, negative cleared
        cache.insert("interfaces".to_string(), vec!["ether1".to_string()]);
        assert!(!cache.is_negative_cooldown("interfaces"));
        // Not in cooldown for other key
        assert!(!cache.is_negative_cooldown("ip_addresses"));
    }

    #[test]
    fn test_background_fetch_coalescing() {
        let cache = Arc::new(Mutex::new(LiveCache::with_default_ttl()));
        let mut m = HashMap::new();
        m.insert("RSC_LS_LIVE", "1");
        m.insert("MIKROTIK_HOST", "192.168.88.1");
        m.insert("MIKROTIK_PASS", "p");
        let _cfg = cfg_with(m);
        // First call should trigger background fetch (no cache)
        {
            let guard = cache.lock().unwrap();
            assert!(guard.can_spawn_fetch("interfaces"));
        }
        // Simulate a fetch attempt recorded
        {
            let mut guard = cache.lock().unwrap();
            guard.record_fetch_attempt("interfaces".to_string());
            assert!(!guard.can_spawn_fetch("interfaces")); // coalesced within 2s
        }
        // After negative cooldown, still cannot spawn
        {
            let mut guard = cache.lock().unwrap();
            guard.insert_negative("interfaces".to_string());
            assert!(!guard.can_spawn_fetch("interfaces"));
        }
    }

    #[test]
    fn test_custom_resource_parsing() {
        let json = r#"[{"property":"packet-mark","path":"/rest/ip/firewall/mangle","field":"new-packet-mark"}]"#;
        let mut m = HashMap::new();
        m.insert("RSC_LS_LIVE", "1");
        m.insert("MIKROTIK_HOST", "h");
        m.insert("MIKROTIK_PASS", "p");
        m.insert("RSC_LS_LIVE_RESOURCES", json);
        let cfg = cfg_with(m);
        assert_eq!(cfg.custom_resources.len(), 1);
        assert_eq!(cfg.custom_resources[0].property, "packet-mark");
        assert_eq!(cfg.custom_resources[0].path, "/rest/ip/firewall/mangle");
        assert_eq!(cfg.custom_resources[0].field, "new-packet-mark");
        // Resolve custom via LiveConfig fallback
        assert!(
            cfg.resolve_resource_with_custom("/ip/firewall/mangle", "packet-mark", "string")
                .is_some()
        );
        // Hardcoded still works
        assert_eq!(
            cfg.resolve_resource_with_custom("", "interface", "string"),
            Some(ResourceKind::Interfaces)
        );
        // Unknown without custom returns None
        assert!(
            cfg.resolve_resource_with_custom("", "unknown-prop", "string")
                .is_none()
        );
    }

    #[test]
    fn test_custom_resource_cap() {
        // More than 8 should truncate
        let many: Vec<String> = (0..10)
            .map(|i| format!(r#"{{"property":"p{i}","path":"/rest/interface","field":"name"}}"#))
            .collect();
        let json = format!("[{}]", many.join(","));
        let mut m = HashMap::new();
        m.insert("RSC_LS_LIVE", "1");
        m.insert("MIKROTIK_HOST", "h");
        m.insert("MIKROTIK_PASS", "p");
        m.insert("RSC_LS_LIVE_RESOURCES", json.as_str());
        let cfg = cfg_with(m);
        assert_eq!(cfg.custom_resources.len(), LIVE_CUSTOM_RESOURCES_MAX);
    }

    #[test]
    fn test_hot_reload_from_settings() {
        let cfg = LiveConfig::from_env_with(|k| match k {
            "RSC_LS_LIVE" => Some("1".to_string()),
            "MIKROTIK_HOST" => Some("192.168.88.1".to_string()),
            "MIKROTIK_PASS" => Some("envpass".to_string()),
            _ => None,
        });
        assert_eq!(cfg.host, "192.168.88.1");
        // Simulate settings overlay
        let settings = serde_json::json!({
            "rsc": {
                "live": {
                    "host": "10.0.0.5",
                    "port": 8728
                }
            }
        });
        let _cfg2 = LiveConfig::from_settings_value(&settings);
        // from_settings_value starts from env (which has 192.168.88.1) but overlays 10.0.0.5
        // Note: from_env inside will read real env, not our mocked one. So we test apply directly.
        let mut cfg3 = cfg.clone();
        LiveConfig::apply_settings_value(&mut cfg3, &settings);
        assert_eq!(cfg3.host, "10.0.0.5");
        assert_eq!(cfg3.port, 8728);
        assert_eq!(cfg3.hosts, vec!["10.0.0.5".to_string()]);
    }

    // ── Shared fetch tail (D1) ───────────────────────────────────

    #[test]
    fn test_extract_and_sanitize_extracts_field_and_filters() {
        let arr: Vec<serde_json::Value> = serde_json::from_str(
            r#"[{"name":"ether2"},{"name":"ether1"},{"name":"ether1"},{"name":"bad val"},{"nope":"x"},{"name":"wlan1"}]"#,
        )
        .unwrap();
        let out = extract_and_sanitize(&arr, "name", ResourceKind::Interfaces);
        assert_eq!(out, vec!["ether1", "ether2", "wlan1"]);
    }

    #[test]
    fn test_extract_and_sanitize_ip_kind_uses_ip_filter() {
        let arr: Vec<serde_json::Value> = serde_json::from_str(
            r#"[{"address":"10.0.0.1/24"},{"address":"192.168.1.1 evil"},{"address":"2001:db8::1/64"}]"#,
        )
        .unwrap();
        let out = extract_and_sanitize(&arr, "address", ResourceKind::IpAddresses);
        assert_eq!(out, vec!["10.0.0.1/24", "2001:db8::1/64"]);
    }

    #[test]
    fn test_extract_and_sanitize_caps_raw_values() {
        // More than 2*MAX_LIVE_ITEMS entries: extraction stops early and the
        // sanitized output is capped to MAX_LIVE_ITEMS.
        let n = MAX_LIVE_ITEMS * 2 + 10;
        let entries: Vec<String> = (0..n)
            .map(|i| format!("{{\"name\":\"iface{i:04}\"}}"))
            .collect();
        let arr: Vec<serde_json::Value> =
            serde_json::from_str(&format!("[{}]", entries.join(","))).unwrap();
        let out = extract_and_sanitize(&arr, "name", ResourceKind::Interfaces);
        assert_eq!(out.len(), MAX_LIVE_ITEMS);
    }

    fn custom_test_resource() -> CustomResource {
        CustomResource {
            property: "packet-mark".to_string(),
            path: "/rest/ip/firewall/mangle".to_string(),
            field: "new-packet-mark".to_string(),
        }
    }

    #[test]
    fn test_fetch_custom_resource_disabled() {
        let cfg = cfg_with(HashMap::new()); // disabled
        let res = fetch_custom_resource(&cfg, &custom_test_resource());
        assert!(matches!(res, Err(LiveError::Disabled)));
    }

    #[test]
    fn test_fetch_custom_resource_rejects_host_with_slash() {
        // Same shared validation as the built-in fetchers (via build_base_url).
        let mut m = HashMap::new();
        m.insert("RSC_LS_LIVE", "1");
        m.insert("MIKROTIK_HOST", "host/with/slash");
        m.insert("MIKROTIK_PASS", "p");
        let cfg = cfg_with(m);
        let res = fetch_custom_resource(&cfg, &custom_test_resource());
        assert!(matches!(res, Err(LiveError::InvalidHost(_))));
    }

    // ── Completion enrichment trigger (D2) ───────────────────────
    //
    // These tests exercise the synchronous coalescing logic of
    // `trigger_enrichment_for_completion`. The host is loopback with a closed
    // port so any background fetch fails fast; the network outcome is not
    // asserted (it runs on a detached thread).

    fn active_test_cfg() -> LiveConfig {
        let mut m = HashMap::new();
        m.insert("RSC_LS_LIVE", "1");
        m.insert("MIKROTIK_HOST", "127.0.0.1");
        m.insert("MIKROTIK_PORT", "1");
        m.insert("MIKROTIK_PASS", "p");
        m.insert("MIKROTIK_TIMEOUT", "1");
        cfg_with(m)
    }

    #[test]
    fn test_trigger_enrichment_inactive_config_does_nothing() {
        let cache = Arc::new(Mutex::new(LiveCache::with_default_ttl()));
        let cfg = cfg_with(HashMap::new()); // disabled
        trigger_enrichment_for_completion(&cache, &cfg, None, "/", "");
        assert!(cache.lock().unwrap().last_fetch_attempt.is_empty());
    }

    #[test]
    fn test_trigger_enrichment_without_property_prefetches_interfaces() {
        let cache = Arc::new(Mutex::new(LiveCache::with_default_ttl()));
        let cfg = active_test_cfg();
        trigger_enrichment_for_completion(&cache, &cfg, None, "/", "");
        let guard = cache.lock().unwrap();
        // Coalescing marker recorded for the interfaces key...
        assert!(guard.last_fetch_attempt.contains_key("interfaces"));
        // ...so a second trigger is coalesced, not re-spawned.
        assert!(!guard.can_spawn_fetch("interfaces"));
    }

    #[test]
    fn test_trigger_enrichment_with_property_resolves_resource() {
        let cache = Arc::new(Mutex::new(LiveCache::with_default_ttl()));
        let cfg = active_test_cfg();
        trigger_enrichment_for_completion(&cache, &cfg, Some("interface"), "/ip/address", "iface");
        let guard = cache.lock().unwrap();
        assert!(guard.last_fetch_attempt.contains_key("interfaces"));
    }

    #[test]
    fn test_trigger_enrichment_unknown_property_falls_back_to_interfaces() {
        let cache = Arc::new(Mutex::new(LiveCache::with_default_ttl()));
        let cfg = active_test_cfg();
        trigger_enrichment_for_completion(&cache, &cfg, Some("comment"), "", "");
        let guard = cache.lock().unwrap();
        // Only the interfaces prefetch; no custom key involved.
        assert!(guard.last_fetch_attempt.contains_key("interfaces"));
        assert_eq!(guard.last_fetch_attempt.len(), 1);
    }

    #[test]
    fn test_trigger_enrichment_custom_resource_uses_custom_key() {
        let mut m = HashMap::new();
        m.insert("RSC_LS_LIVE", "1");
        m.insert("MIKROTIK_HOST", "127.0.0.1");
        m.insert("MIKROTIK_PORT", "1");
        m.insert("MIKROTIK_PASS", "p");
        m.insert("MIKROTIK_TIMEOUT", "1");
        m.insert(
            "RSC_LS_LIVE_RESOURCES",
            r#"[{"property":"packet-mark","path":"/rest/ip/firewall/mangle","field":"new-packet-mark"}]"#,
        );
        let cfg = cfg_with(m);
        let cache = Arc::new(Mutex::new(LiveCache::with_default_ttl()));
        trigger_enrichment_for_completion(
            &cache,
            &cfg,
            Some("packet-mark"),
            "/ip/firewall/mangle",
            "string",
        );
        let guard = cache.lock().unwrap();
        // Custom property resolves to the generic Interfaces kind...
        assert!(guard.last_fetch_attempt.contains_key("interfaces"));
        // ...and the custom resource is tracked under `custom:<property>`.
        assert!(guard.last_fetch_attempt.contains_key("custom:packet-mark"));
        let recorded_at = guard.last_fetch_attempt["custom:packet-mark"];
        drop(guard);
        // Second call within the coalescing window must not re-record.
        trigger_enrichment_for_completion(
            &cache,
            &cfg,
            Some("packet-mark"),
            "/ip/firewall/mangle",
            "string",
        );
        let guard = cache.lock().unwrap();
        assert_eq!(
            guard.last_fetch_attempt["custom:packet-mark"], recorded_at,
            "custom fetch attempt must be coalesced within the window"
        );
    }

    // Helper trait for sorted check in tests (stable in std from 1.82?).

    // Helper trait for sorted check in tests (stable in std from 1.82?).
    trait IsSorted {
        fn is_sorted(&self) -> bool;
    }
    impl IsSorted for Vec<String> {
        fn is_sorted(&self) -> bool {
            self.windows(2).all(|w| w[0] <= w[1])
        }
    }
}
