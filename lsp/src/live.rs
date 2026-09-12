// ── Live device data (opt-in, in-memory only) ────────────────────────────
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

// Subsystem split: config/net/cache/fetch live in the sibling
// `live_*` modules, re-exported below so `crate::live::…` paths
// keep resolving unchanged (server.rs and all tests untouched).
// Explicit (no globs): the facade surface is reviewable, and any
// newly unused name warns precisely instead of rotting silently.

#[allow(unused_imports)]
pub(crate) use crate::live_cache::{
    CachedValue, LiveCache, ResourceKind, filter_ip_value, filter_value,
    get_cached_or_fetch_background, live_resource_for_menu_property,
    live_resource_values_for_property, sanitize_resource_values, trigger_enrichment_for_completion,
};
#[allow(unused_imports)]
pub(crate) use crate::live_config::{
    CustomResource, LiveConfig, is_valid_custom_path, parse_fingerprint, resolve_scheme,
    resolve_scheme_with_legacy, validate_user,
};
#[allow(unused_imports)]
pub(crate) use crate::live_fetch::{
    FetchPermitGuard, MAX_CONCURRENT_FETCHES, extract_and_sanitize, fetch_custom_resource,
    fetch_resource, get_cached_agent_for_config, parse_pem_certs, try_acquire_fetch_permit,
};
#[allow(unused_imports)]
pub(crate) use crate::live_net::{
    LiveError, MAX_CA_FILE_BYTES, build_base_url_with_allow, build_custom_rest_url, build_rest_url,
    denied_reason_for_ip, embedded_ipv4, extract_spki_der, format_host_for_url, is_bad_ca,
    is_ipv6_transition_prefix, is_loopback_or_private, is_non_canonical_numeric_host,
    is_normalized_loopback_or_private, is_normalized_ssrf_denied, is_ssrf_denied_host,
    live_identity_changed, normalized_host_ip, read_ca_bundle, resolve_and_validate_host, sha256,
    sha256_block, spki_sha256, validate_host, validate_host_with_allow,
};

// Test-only helpers stay reachable through the same facade path so
// `use crate::live::*;` in `lsp/src/tests/` keeps working unchanged.
#[cfg(test)]
pub(crate) use crate::live_cache::{
    is_live_property, live_resource_for_property, live_values_for_property, sanitize_values,
};
#[cfg(test)]
pub(crate) use crate::live_config::{legacy_http_shim_allowed_with, with_settings_transport_env};
#[cfg(test)]
pub(crate) use crate::live_fetch::{
    PinnedAddrs, build_insecure_agent, fetch_interfaces, get_cached_agent,
};
