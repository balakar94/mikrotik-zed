// Live security audit (identity, deny reasons, CA).
// Copied (not moved) from `lsp/src/live.rs` (`mod tests` L3481-3489, L4992-5000, L5125-5237); the original block is
// left untouched. `use super::*` is adapted to `use crate::live::*;` for the new location.
use crate::caps::*;
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
fn secure_base_cfg() -> LiveConfig {
    LiveConfig::from_env_with(|k| match k {
        "RSC_LS_LIVE" => Some("1".to_string()),
        "MIKROTIK_HOST" => Some("router.local".to_string()),
        "MIKROTIK_PASS" => Some("envpass".to_string()),
        _ => None,
    })
}

// ── Security audit F1–F8 ─────────────────────────────────────

#[test]
fn test_live_identity_changed_covers_pin_and_ca() {
    // F4: pin / pin-validity / CA-bundle rotations must invalidate.
    let base = secure_base_cfg();
    assert!(!live_identity_changed(&base, &base.clone()));
    let mut pin_changed = base.clone();
    pin_changed.fingerprint = Some([0xabu8; 32]);
    assert!(live_identity_changed(&base, &pin_changed));
    let mut invalid_changed = base.clone();
    invalid_changed.fingerprint_invalid = true;
    assert!(live_identity_changed(&base, &invalid_changed));
    let mut ca_changed = base.clone();
    ca_changed.ca_file = "/tmp/ca.pem".to_string();
    assert!(live_identity_changed(&base, &ca_changed));
}

#[test]
fn test_denied_reason_for_ip_vectors() {
    use std::net::IpAddr;
    // F1 test vectors: unconditional denials regardless of loopback flag.
    for bad in ["169.254.169.254", "169.254.1.1", "fe80::1", "0.0.0.0", "::"] {
        let addr: IpAddr = bad.parse().unwrap();
        assert!(
            denied_reason_for_ip(addr, true).is_some(),
            "must deny even when loopback allowed: {bad}"
        );
        assert!(denied_reason_for_ip(addr, false).is_some());
    }
    // Loopback/private denied only without the flag.
    for gated in ["127.0.0.1", "10.0.0.5", "192.168.1.1", "::1"] {
        let addr: IpAddr = gated.parse().unwrap();
        assert!(denied_reason_for_ip(addr, false).is_some());
        assert!(denied_reason_for_ip(addr, true).is_none());
    }
    // Public addresses pass in both modes.
    for ok in ["8.8.8.8", "1.1.1.1", "2001:db8::1"] {
        let addr: IpAddr = ok.parse().unwrap();
        assert!(denied_reason_for_ip(addr, false).is_none());
        assert!(denied_reason_for_ip(addr, true).is_none());
    }
    // IPv4-mapped metadata is denied via unmapping.
    let mapped: IpAddr = "::ffff:169.254.169.254".parse().unwrap();
    assert!(denied_reason_for_ip(mapped, true).is_some());
}

#[test]
fn test_resolve_and_validate_host_literals_fail_closed() {
    // F1: IP literals resolve locally (no DNS traffic); policy applies.
    assert!(resolve_and_validate_host("8.8.8.8", 443, false).is_ok());
    assert!(resolve_and_validate_host("169.254.169.254", 443, true).is_err());
    assert!(resolve_and_validate_host("127.0.0.1", 443, false).is_err());
    assert!(resolve_and_validate_host("127.0.0.1", 443, true).is_ok());
    assert!(resolve_and_validate_host("[::1]", 443, false).is_err());
    assert!(resolve_and_validate_host("fe80::1", 443, true).is_err());
    // Unresolvable names fail closed (no credentials on DNS failure).
    assert!(resolve_and_validate_host("nonexistent.invalid", 443, false).is_err());
    assert!(resolve_and_validate_host("", 443, false).is_err());
}

#[test]
fn test_custom_path_rejects_rest_prefix_squatting() {
    // F7: only `/rest` exactly or `/rest/...` pass.
    assert!(is_valid_custom_path("/rest"));
    assert!(is_valid_custom_path("/rest/interface"));
    assert!(is_valid_custom_path("/rest/ip/firewall/mangle"));
    for bad in [
        "/restful",
        "/restx",
        "/rest-evil",
        "/rest..",
        "/api/rest",
        "rest/interface",
        "/rest//interface",
        "/rest/../etc",
    ] {
        assert!(!is_valid_custom_path(bad), "must reject: {bad:?}");
    }
}

#[test]
fn test_ca_bundle_missing_and_oversize_fail_closed() {
    // F8: missing files are fail-closed None (callers keep verifying).
    assert!(read_ca_bundle("/nonexistent/ca-bundle.pem").is_none());
    assert!(read_ca_bundle("").is_none());
    // Oversize bundles are rejected without reading past the cap.
    let dir = std::env::temp_dir().join("rsc-ls-ca-test");
    let _ = std::fs::create_dir_all(&dir);
    let big = dir.join("big-ca.pem");
    let bytes = vec![b'A'; (MAX_CA_FILE_BYTES + 1) as usize];
    std::fs::write(&big, &bytes).unwrap();
    let key = big.to_string_lossy().to_string();
    assert!(read_ca_bundle(&key).is_none());
    // Negative cache: second call short-circuits without I/O.
    assert!(is_bad_ca(&key) || read_ca_bundle(&key).is_none());
    let _ = std::fs::remove_file(&big);
}

#[test]
fn test_network_error_redaction_at_live_boundary() {
    // F6: password + Basic material never survives into a stored error.
    use base64::Engine;
    let pass = "live-s3cret!";
    let user = "admin";
    let basic =
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}").as_bytes());
    let raw = format!("connection to https://host failed auth={basic} pw={pass}");
    let safe = crate::logging::redact_secrets(&raw, pass, user);
    assert!(!safe.contains(pass));
    assert!(!safe.contains(&basic));
    assert!(safe.contains("[REDACTED]"));
}

#[test]
fn test_dns_revalidation_blocks_rebound_loopback() {
    // F1 TOCTOU: a name that looks benign at config time must still be
    // refused when revalidation resolves it to loopback at fetch time.
    // `resolve_and_validate_host` has no injectable resolver (it calls the
    // system resolver directly), so the revalidation half is pinned through
    // its pure classifier plus IP literals that resolve locally with no
    // DNS traffic.
    assert!(resolve_and_validate_host("8.8.8.8", 443, false).is_ok());
    let rebound: std::net::IpAddr = "127.0.0.1".parse().unwrap();
    assert!(
        denied_reason_for_ip(rebound, false).is_some(),
        "re-resolved loopback must be denied without the loopback opt-in"
    );
    assert!(
        resolve_and_validate_host("127.0.0.1", 443, false).is_err(),
        "fetch gate must fail closed when revalidation yields loopback"
    );
}
