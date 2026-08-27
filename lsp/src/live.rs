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
    LIVE_FETCH_BLOCKING_TIMEOUT_SECS, LIVE_TIMEOUT_SECS, LIVE_TTL_SECS, MAX_CACHE_ENTRIES,
    MAX_LIVE_ITEMS, MAX_LIVE_RESPONSE_BYTES, MAX_LIVE_VALUE_LEN,
};
use crate::logging::{log_debug, log_info, log_warn};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ── LiveConfig ───────────────────────────────────────────────────

/// Live device connection configuration, parsed from the environment.
///
/// Mirrors `scripts/mikrotik-deploy.py` semantics so the same env vars
/// work for both the deploy companion and the language server.
#[derive(Clone)]
pub struct LiveConfig {
    /// Opt-in flag: `RSC_LS_LIVE=1` or `MIKROTIK_LIVE=1`.
    pub enabled: bool,
    /// Device host/IP (`MIKROTIK_HOST`). Empty when not set.
    pub host: String,
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
}

impl std::fmt::Debug for LiveConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveConfig")
            .field("enabled", &self.enabled)
            .field("host", &self.host)
            .field("user", &self.user)
            .field("pass", &"[REDACTED]")
            .field("port", &self.port)
            .field("ssl_verify", &self.ssl_verify)
            .field("force_http", &self.force_http)
            .field("timeout_secs", &self.timeout_secs)
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

        let host = get("MIKROTIK_HOST").unwrap_or_default().trim().to_string();
        let user_raw = get("MIKROTIK_USER").unwrap_or_default();
        let user = if user_raw.trim().is_empty() {
            "admin".to_string()
        } else {
            user_raw.trim().to_string()
        };
        let pass = get("MIKROTIK_PASS").unwrap_or_default();

        // Port: mirror _env_int with default 443 and warning on bad input.
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

        LiveConfig {
            enabled,
            host,
            user,
            pass,
            port,
            ssl_verify,
            force_http,
            timeout_secs,
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

    /// Resolve the REST scheme, mirroring `scripts/mikrotik-deploy.py::resolve_scheme`.
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
                "live enabled host={} port={} scheme={} user={} ssl_verify={} timeout={}s",
                self.host,
                self.port,
                self.scheme(),
                self.user,
                self.ssl_verify,
                self.timeout_secs
            );
        } else if self.enabled {
            // Opt-in was requested but required vars missing/invalid.
            log_info!(
                "live enabled but inactive — missing/invalid MIKROTIK_HOST or MIKROTIK_PASS (opt-in via RSC_LS_LIVE=1)"
            );
        } else {
            log_info!("live disabled (opt-in via RSC_LS_LIVE=1 or MIKROTIK_LIVE=1)");
        }
    }
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

/// Resolve the REST URL scheme, mirroring deploy companion `resolve_scheme`.
///
/// `port`: target port.
/// `force_http`: `MIKROTIK_HTTP=1`.
/// `ssl_verify`: true when verification is enabled; false when `MIKROTIK_SSL=0`.
pub(crate) fn resolve_scheme(port: u16, force_http: bool, ssl_verify: bool) -> &'static str {
    let no_ssl_verify = !ssl_verify;
    if !force_http && no_ssl_verify && port != 443 && port != 8729 {
        return "http";
    }
    if force_http { "http" } else { "https" }
}

/// Validate `host` per defensive rules.
///
/// - non-empty, max 253 chars
/// - no null bytes, no control chars
/// - no URI delimiters that would alter URL parsing (`@`, `?`, `#`, ` `, `%`)
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
    Ok(())
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
}

impl LiveCache {
    /// Create a cache with explicit TTL.
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            ttl,
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
            key,
            CachedValue {
                values: vals,
                fetched_at: Instant::now(),
            },
        );
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
            key,
            CachedValue {
                values: vals,
                fetched_at: at,
            },
        );
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

/// Generic blocking fetch or cache read for any ResourceKind with timeout.
pub fn get_cached_or_fetch_resource_blocking_with_timeout(
    cache: &Arc<Mutex<LiveCache>>,
    config: &LiveConfig,
    resource: ResourceKind,
    blocking_timeout: Duration,
) -> Option<Vec<String>> {
    if !config.is_active() {
        return None;
    }
    let key = resource.cache_key();
    // Fast path: fresh cache.
    {
        let guard = cache.lock().expect("live cache lock poisoned");
        if let Some(vals) = guard.try_get_cached(key) {
            log_debug!("live cache hit (fresh) for {key}");
            return Some(vals);
        }
    }
    log_debug!(
        "live cache miss or stale — fetching {:?} from {}:{} (scheme={}, timeout={}s)",
        resource,
        config.host,
        config.port,
        config.scheme(),
        config.timeout_secs
    );
    let fetch_timeout_secs = std::cmp::min(config.timeout_secs, blocking_timeout.as_secs().max(1));
    let mut fetch_config = config.clone();
    fetch_config.timeout_secs = fetch_timeout_secs;

    let start = Instant::now();
    let result = fetch_resource(&fetch_config, resource);
    let elapsed = start.elapsed();
    if elapsed > blocking_timeout + Duration::from_millis(200) {
        log_warn!(
            "live fetch {:?} exceeded blocking budget (elapsed {:?} > {:?})",
            resource,
            elapsed,
            blocking_timeout
        );
    }
    match result {
        Ok(values) => {
            if values.is_empty() {
                log_debug!("live fetch {:?} returned empty set", resource);
                return None;
            }
            log_debug!("live fetch {:?} ok: {} items", resource, values.len());
            let mut guard = cache.lock().expect("live cache lock poisoned");
            guard.insert(key.to_string(), values.clone());
            Some(values)
        }
        Err(e) => {
            log_warn!("live fetch {:?} failed: {e}", resource);
            None
        }
    }
}

/// Convenience wrapper for fetching a specific resource blocking.
pub fn get_cached_or_fetch_resource_blocking(
    cache: &Arc<Mutex<LiveCache>>,
    config: &LiveConfig,
    resource: ResourceKind,
) -> Option<Vec<String>> {
    get_cached_or_fetch_resource_blocking_with_timeout(
        cache,
        config,
        resource,
        Duration::from_secs(LIVE_FETCH_BLOCKING_TIMEOUT_SECS),
    )
}

/// Try to return a cached live value, or fetch blocking with a timeout (default interfaces).
pub fn get_cached_or_fetch_blocking_with_timeout(
    cache: &Arc<Mutex<LiveCache>>,
    config: &LiveConfig,
    blocking_timeout: Duration,
) -> Option<Vec<String>> {
    get_cached_or_fetch_resource_blocking_with_timeout(
        cache,
        config,
        ResourceKind::Interfaces,
        blocking_timeout,
    )
}

/// Convenience wrapper using the default blocking timeout (2 s) for interfaces.
pub fn get_cached_or_fetch_blocking(
    cache: &Arc<Mutex<LiveCache>>,
    config: &LiveConfig,
) -> Option<Vec<String>> {
    get_cached_or_fetch_blocking_with_timeout(
        cache,
        config,
        Duration::from_secs(LIVE_FETCH_BLOCKING_TIMEOUT_SECS),
    )
}

// ── Fetch ────────────────────────────────────────────────────────

/// Fetch live data for a specific resource kind from the RouterOS REST API.
pub fn fetch_resource(
    config: &LiveConfig,
    resource: ResourceKind,
) -> Result<Vec<String>, LiveError> {
    if !config.is_active() {
        return Err(LiveError::Disabled);
    }
    validate_host(&config.host)?;
    if config.port == 0 {
        return Err(LiveError::InvalidPort("port 0".to_string()));
    }

    let scheme = config.scheme();
    if config.host.contains('/') || config.host.contains('\\') {
        return Err(LiveError::InvalidHost(
            "host contains path separator".to_string(),
        ));
    }
    let url = format!(
        "{}://{}:{}{}",
        scheme,
        config.host,
        config.port,
        resource.rest_path()
    );
    log_debug!(
        "live fetch_resource kind={:?} url={} user={} timeout={}s",
        resource,
        url,
        config.user,
        config.timeout_secs
    );

    if !config.ssl_verify {
        log_debug!(
            "live ssl_verify=false — TLS verification would be disabled (MVP keeps default verifier)"
        );
    }
    let timeout = Duration::from_secs(config.timeout_secs.clamp(1, 30));
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();

    let credentials = format!("{}:{}", config.user, config.pass);
    let encoded = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        credentials.as_bytes(),
    );
    let auth_header = format!("Basic {encoded}");

    let resp: Result<ureq::Response, ureq::Error> = agent
        .get(&url)
        .set("Accept", "application/json")
        .set("Authorization", &auth_header)
        .call();

    let response: ureq::Response = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, _)) => {
            return Err(LiveError::Status(code));
        }
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
    let arr = json
        .as_array()
        .ok_or_else(|| LiveError::Parse("expected JSON array".to_string()))?;

    let field_name = resource.json_field();
    let mut raw_values: Vec<String> = Vec::new();
    for entry in arr {
        if let Some(obj) = entry.as_object()
            && let Some(val) = obj.get(field_name)
            && let Some(val_str) = val.as_str()
        {
            raw_values.push(val_str.to_string());
        }
        if raw_values.len() >= MAX_LIVE_ITEMS * 2 {
            break;
        }
    }

    let cleaned = sanitize_resource_values(raw_values, resource);
    if cleaned.is_empty() && !arr.is_empty() {
        log_warn!(
            "live fetch parsed 0 valid values for {:?} from {} entries",
            resource,
            arr.len()
        );
    }
    Ok(cleaned)
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
        let res = get_cached_or_fetch_blocking(&cache, &cfg);
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
        let res = get_cached_or_fetch_blocking(&cache, &cfg);
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
}

// Helper trait for sorted check in tests (stable in std from 1.82?).
#[cfg(test)]
trait IsSorted {
    fn is_sorted(&self) -> bool;
}
#[cfg(test)]
impl IsSorted for Vec<String> {
    fn is_sorted(&self) -> bool {
        self.windows(2).all(|w| w[0] <= w[1])
    }
}
