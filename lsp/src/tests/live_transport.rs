// Transport hardening and user validation.
// Copied (not moved) from `lsp/src/live.rs`; the original block is
// left untouched. `use super::*` is adapted to `use crate::live::*;` for the new location.
use crate::live::*;
use std::collections::HashMap;

fn cfg_with(mut map: HashMap<&str, &str>) -> LiveConfig {
    // Tests historically use private hosts (192.168.88.1) which would now be denied by default.
    // To keep those fixtures honest while still exercising the new SSRF flag, inject
    // RSC_LS_LIVE_ALLOW_LOOPBACK=1 unless the test explicitly sets it.
    if !map.contains_key("RSC_LS_LIVE_ALLOW_LOOPBACK") {
        map.insert("RSC_LS_LIVE_ALLOW_LOOPBACK", "1");
    }
    LiveConfig::from_env_with(|k| map.get(k).map(|v| v.to_string()))
}
// ── Transport hardening ──────────────────────────────────────────────────

fn secure_base_cfg() -> LiveConfig {
    LiveConfig::from_env_with(|k| match k {
        "RSC_LS_LIVE" => Some("1".to_string()),
        "MIKROTIK_HOST" => Some("router.local".to_string()),
        "MIKROTIK_PASS" => Some("envpass".to_string()),
        _ => None,
    })
}

#[test]
fn test_transport_downgrades_ignored_without_opt_in() {
    let mut cfg = secure_base_cfg();
    assert!(cfg.ssl_verify);
    assert!(!cfg.force_http);
    assert!(!cfg.allow_loopback);
    assert!(cfg.custom_resources.is_empty());
    let settings = serde_json::json!({
        "rsc": {
            "live": {
                "ssl_verify": false,
                "force_http": true,
                "allow_loopback": true,
                "custom_resources": [{"property":"p0","path":"/rest/interface","field":"name"}]
            }
        }
    });
    LiveConfig::apply_settings_value_with_transport(&mut cfg, &settings, false);
    assert!(
        cfg.ssl_verify,
        "ssl downgrade must be ignored without opt-in"
    );
    assert!(!cfg.force_http, "force_http must be ignored without opt-in");
    assert!(
        !cfg.allow_loopback,
        "allow_loopback must be ignored without opt-in"
    );
    assert!(
        cfg.custom_resources.is_empty(),
        "custom_resources must be ignored without opt-in"
    );
    assert_eq!(cfg.pass, "envpass");
}

#[test]
fn test_transport_downgrades_allowed_with_opt_in() {
    let mut cfg = secure_base_cfg();
    let settings = serde_json::json!({
        "rsc": {
            "live": {
                "ssl_verify": false,
                "force_http": true,
                "allow_loopback": true,
                "custom_resources": [{"property":"p0","path":"/rest/interface","field":"name"}]
            }
        }
    });
    LiveConfig::apply_settings_value_with_transport(&mut cfg, &settings, true);
    assert!(!cfg.ssl_verify);
    assert!(cfg.force_http);
    assert!(cfg.allow_loopback);
    assert_eq!(cfg.custom_resources.len(), 1);
    assert_eq!(cfg.custom_resources[0].property, "p0");
}

#[test]
fn test_transport_hardening_allowed_without_opt_in() {
    // Hardening direction (verify on, http off, loopback off) stays allowed.
    let mut cfg = LiveConfig::from_env_with(|k| match k {
        "RSC_LS_LIVE" => Some("1".to_string()),
        "MIKROTIK_HOST" => Some("router.local".to_string()),
        "MIKROTIK_PASS" => Some("envpass".to_string()),
        "MIKROTIK_SSL" => Some("0".to_string()),
        "MIKROTIK_HTTP" => Some("1".to_string()),
        _ => None,
    });
    assert!(!cfg.ssl_verify);
    assert!(cfg.force_http);
    let settings = serde_json::json!({
        "rsc": {"live": {"ssl_verify": true, "force_http": false, "allow_loopback": false}}
    });
    LiveConfig::apply_settings_value_with_transport(&mut cfg, &settings, false);
    assert!(cfg.ssl_verify);
    assert!(!cfg.force_http);
    assert!(!cfg.allow_loopback);
}

#[test]
fn test_validate_user_rejects_injection() {
    assert_eq!(validate_user("admin"), Some("admin".to_string()));
    assert_eq!(
        validate_user("net-ops_1.router"),
        Some("net-ops_1.router".to_string())
    );
    assert!(validate_user("a:b").is_none());
    assert!(validate_user("evil\nadmin").is_none());
    assert!(validate_user("evil\radmin").is_none());
    assert!(validate_user("a@b").is_none());
    assert!(validate_user("a%b").is_none());
    assert!(validate_user("a\0b").is_none());
    assert!(validate_user("a b").is_none());
    assert!(validate_user("").is_none());
    assert!(validate_user(&"a".repeat(65)).is_none());
    assert_eq!(validate_user(&"a".repeat(64)).map(|s| s.len()), Some(64));
}

#[test]
fn test_invalid_user_falls_back_to_admin() {
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "router.local");
    m.insert("MIKROTIK_PASS", "p");
    m.insert("MIKROTIK_USER", "admin:evil\ninjected");
    let cfg = LiveConfig::from_env_with(|k| m.get(k).map(|v| v.to_string()));
    assert_eq!(cfg.user, "admin");
    // Settings overlay with newline injection also falls back to admin.
    let mut cfg2 = secure_base_cfg();
    let settings = serde_json::json!({
        "rsc": {"live": {"user": "evil\ninjected"}}
    });
    LiveConfig::apply_settings_value_with_transport(&mut cfg2, &settings, false);
    assert_eq!(cfg2.user, "admin");
}

#[test]
fn test_sanitize_for_log_has_no_raw_newline() {
    let out = crate::logging::sanitize_for_log("evil\ninjected\rhost");
    assert!(!out.contains('\n'));
    assert!(!out.contains('\r'));
    assert_eq!(out, "evilinjectedhost");
    let long = "x".repeat(500);
    assert_eq!(crate::logging::sanitize_for_log(&long).chars().count(), 128);
}
