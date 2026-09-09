// ── Live cache ─────────────────────────────────────────────
//
// `ResourceKind` + value filtering and the TTL `LiveCache`. Extracted verbatim from `live.rs`; re-exported there
// so `crate::live::…` paths keep resolving unchanged.

use crate::caps::{
    LIVE_FETCH_BLOCKING_TIMEOUT_SECS, LIVE_NEGATIVE_TTL_SECS, LIVE_TTL_SECS, MAX_CACHE_ENTRIES,
    MAX_LIVE_ITEMS, MAX_LIVE_VALUE_LEN,
};
use crate::live_config::{CustomResource, LiveConfig};
use crate::live_fetch::{
    FetchPermitGuard, MAX_CONCURRENT_FETCHES, fetch_custom_resource, fetch_resource,
    try_acquire_fetch_permit,
};
use crate::logging::{log_debug, log_info, log_warn, redact_secrets, sanitize_for_log};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
