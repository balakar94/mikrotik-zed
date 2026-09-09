// Workspace host overlays and empty-result caching.
// Copied (not moved) from `lsp/src/live.rs` (`mod tests` L3481-3489, L4835-4989, L4992-5000); the original block is
// left untouched. `use super::*` is adapted to `use crate::live::*;` for the new location.
use crate::live::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

fn cfg_with(mut map: HashMap<&str, &str>) -> LiveConfig {
    // Tests historically use private hosts (192.168.88.1) which would now be denied by default.
    // To keep those fixtures honest while still exercising the new SSRF flag, inject
    // RSC_LS_LIVE_ALLOW_LOOPBACK=1 unless the test explicitly sets it.
    if !map.contains_key("RSC_LS_LIVE_ALLOW_LOOPBACK") {
        map.insert("RSC_LS_LIVE_ALLOW_LOOPBACK", "1");
    }
    LiveConfig::from_env_with(|k| map.get(k).map(|v| v.to_string()))
}
#[test]
fn test_empty_fetch_results_cached() {
    // Endpoints returning `[]` must not re-hit the network on every
    // completion: a fresh empty entry short-circuits via try_get_cached.
    let mut cache = LiveCache::new(Duration::from_secs(60));
    cache.insert("interfaces".to_string(), Vec::new());
    let cached = cache.try_get_cached("interfaces");
    assert!(cached.is_some(), "empty results must be cached");
    assert!(cached.unwrap().is_empty());

    // Through the stale-while-revalidate entry point, the fresh empty hit
    // returns without recording a new fetch attempt.
    let shared = Arc::new(Mutex::new(LiveCache::with_default_ttl()));
    {
        let mut guard = shared.lock().unwrap();
        guard.insert("interfaces".to_string(), Vec::new());
    }
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "192.168.88.1");
    m.insert("MIKROTIK_PASS", "p");
    let cfg = cfg_with(m);
    let res = get_cached_or_fetch_background(&shared, &cfg, ResourceKind::Interfaces);
    assert!(res.is_some(), "fresh empty cache must hit");
    assert!(res.unwrap().is_empty());
    assert!(
        shared.lock().unwrap().last_fetch_attempt.is_empty(),
        "fresh empty hit must not spawn a fetch"
    );
}

#[test]
fn test_workspace_host_overlay_applies() {
    // A scoped workspace object that changes the host overlays cleanly
    // (no panic) — but ONLY with the RSC_LS_ALLOW_SETTINGS_TRANSPORT=1
    // opt-in (F2 default deny: settings must not redirect credentials).
    let mut cfg = LiveConfig::from_env_with(|k| match k {
        "RSC_LS_LIVE" => Some("1".to_string()),
        "MIKROTIK_HOST" => Some("192.168.88.1".to_string()),
        "MIKROTIK_PASS" => Some("envpass".to_string()),
        _ => None,
    });
    assert_eq!(cfg.host, "192.168.88.1");
    let settings = serde_json::json!({
        "rsc": {
            "live": {
                "host": "attacker.example.com"
            }
        }
    });
    // Default deny: host unchanged without opt-in.
    LiveConfig::apply_settings_value_with_transport(&mut cfg, &settings, false);
    assert_eq!(cfg.host, "192.168.88.1");
    // Opt-in: overlay applies with a WARN (warn-not-block).
    LiveConfig::apply_settings_value_with_transport(&mut cfg, &settings, true);
    assert_eq!(cfg.host, "attacker.example.com");
    assert_eq!(cfg.hosts, vec!["attacker.example.com".to_string()]);
    // Env-only secrets are untouched by the overlay.
    assert_eq!(cfg.pass, "envpass");
}

#[test]
fn test_workspace_host_overlay_denied_without_opt_in() {
    // F2: host AND user from settings are denied by default; the exact
    // env name gates them.
    let mut cfg = secure_base_cfg();
    let before_host = cfg.host.clone();
    let before_user = cfg.user.clone();
    let settings = serde_json::json!({
        "rsc": {"live": {"host": "evil.example.com", "user": "intruder"}}
    });
    LiveConfig::apply_settings_value_with_transport(&mut cfg, &settings, false);
    assert_eq!(cfg.host, before_host);
    assert_eq!(cfg.user, before_user);
    // Opt-in applies both.
    LiveConfig::apply_settings_value_with_transport(&mut cfg, &settings, true);
    assert_eq!(cfg.host, "evil.example.com");
    assert_eq!(cfg.user, "intruder");
}

#[test]
fn test_workspace_multi_host_overlay_applies() {
    // Multi-host (`hosts`) overlay changes the effective target list and
    // primary host — with the transport opt-in (F2 default deny).
    let mut cfg = LiveConfig::from_env_with(|k| match k {
        "RSC_LS_LIVE" => Some("1".to_string()),
        "MIKROTIK_HOST" => Some("192.168.88.1".to_string()),
        "MIKROTIK_PASS" => Some("envpass".to_string()),
        _ => None,
    });
    let settings = serde_json::json!({
        "rsc": {
            "live": {
                "host": "attacker.example.com, 10.0.0.2"
            }
        }
    });
    LiveConfig::apply_settings_value_with_transport(&mut cfg, &settings, true);
    assert_eq!(cfg.host, "attacker.example.com");
    assert_eq!(
        cfg.hosts,
        vec!["attacker.example.com".to_string(), "10.0.0.2".to_string()]
    );
}

#[test]
fn test_ssrf_denies_metadata_aliases() {
    // Minimal exact-host denials beyond `metadata.google.internal`.
    // Case-insensitive exact matches are denied.
    for bad in [
        "metadata.google",
        "METADATA.GOOGLE",
        "Metadata.Google",
        "metadata.goog",
        "METADATA.GOOG",
        "[metadata.google]",
    ] {
        assert!(
            is_ssrf_denied_host(bad),
            "metadata alias should be denied: {bad:?}"
        );
        assert!(
            validate_host_with_allow(bad, true).is_err(),
            "metadata alias should fail validation: {bad:?}"
        );
    }
    // No over-blocking: normal hosts stay accepted.
    for ok in [
        "google.com",
        "metadata.google.example.com",
        "example-metadata.goog.example.com",
        "router.local",
        "8.8.8.8",
    ] {
        assert!(
            !is_ssrf_denied_host(ok),
            "normal host must not be denied: {ok:?}"
        );
    }
    assert!(validate_host_with_allow("google.com", false).is_ok());
    assert!(validate_host_with_allow("router.local", false).is_ok());
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

fn secure_base_cfg() -> LiveConfig {
    LiveConfig::from_env_with(|k| match k {
        "RSC_LS_LIVE" => Some("1".to_string()),
        "MIKROTIK_HOST" => Some("router.local".to_string()),
        "MIKROTIK_PASS" => Some("envpass".to_string()),
        _ => None,
    })
}
