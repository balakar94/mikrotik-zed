//! Diagnostics — typed validators.
use crate::diagnostics::severity;
use crate::diagnostics::*;
use crate::menus::MenuData;
use std::sync::Arc;
fn validator_data() -> Arc<MenuData> {
    Arc::new(MenuData::from_toml_str(
        r#"
[[menus]]
path = "/validator/typed"
type = "Directory"
[[menus.arguments]]
name = "flag"
type = "bool"
required = true
[[menus.arguments]]
name = "count"
type = "num"
[[menus.arguments]]
name = "uptime"
type = "time"
[[menus.arguments]]
name = "hw"
type = "macAddr"
[[menus.arguments]]
name = "peer"
type = "ipAddr"
[[menus.arguments]]
name = "bits"
type = "ubit (1Mbps, 2Mbps)"
[[menus.arguments]]
name = "freeform"
type = ""
[[menus]]
path = "/validator/props"
type = "Directory"
[[menus.arguments]]
name = "name"
type = "string"
required = true
[[menus.arguments]]
name = "temp"
type = "string"
unset = true
[[menus.arguments]]
name = "fixed"
type = "string"
unset = false
[[menus.read_only]]
name = "serial"
type = "string"
description = "factory serial"
[[menus]]
path = "/validator/chain"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum"
[[menus]]
path = "/validator/filter"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum"
[[menus.arguments]]
name = "action"
type = "enum (accept | drop)"
"#,
    ))
}

/// Severity contract shared by the validator diagnostic pins.
///
/// Pre-existing families keep their current severities; ANY new code
/// (typed validators, unset guard, read-only-write notice, future rules)
/// must be Hint (severity 4) or Information (severity 3) — never a new
/// Warning/Error. Information is accepted here because the
/// read-only-write notice is specified as Info; the dedicated
/// read-only test additionally requires Information exactly.
fn assert_validator_severity_contract(diags: &[crate::diagnostics::Diagnostic]) {
    const WARNING_CODES: &[&str] = &[
        "unknown-menu",
        "unknown-property",
        "missing-required",
        "duplicate-property",
        "unknown-command",
        "invalid-enum-value",
    ];
    const ERROR_CODES: &[&str] = &["unclosed-brace", "unmatched-brace", "unclosed-quote"];
    for d in diags {
        match d.code.as_deref() {
            Some(c) if WARNING_CODES.contains(&c) => {
                assert_eq!(d.severity, Some(severity::WARNING), "code {c}")
            }
            Some(c) if ERROR_CODES.contains(&c) => {
                assert_eq!(d.severity, Some(severity::ERROR), "code {c}")
            }
            Some("truncated") => {
                assert_eq!(d.severity, Some(severity::INFORMATION), "truncated hint")
            }
            Some(other) => assert!(
                d.severity == Some(severity::HINT) || d.severity == Some(severity::INFORMATION),
                "Validator diagnostic '{other}' must be Hint or Information, got {:?}",
                d.severity,
            ),
            None => panic!("every rsc-ls diagnostic carries a code, got {d:?}"),
        }
        assert_eq!(d.source.as_deref(), Some("rsc-ls"), "source tag contract");
    }
}

// ── Typed validators: Hint-only ────────────────────────────────────

fn typed_data() -> MenuData {
    MenuData::from_toml_str(
        r#"
[[menus]]
path = "/demo/typed"
type = "Directory"
[[menus.arguments]]
name = "flag"
type = "bool"
[[menus.arguments]]
name = "count"
type = "num"
[[menus.arguments]]
name = "period"
type = "time"
[[menus.arguments]]
name = "mac"
type = "macAddr"
[[menus.arguments]]
name = "addr"
type = "ipAddr"
[[menus.arguments]]
name = "prefix"
type = "ipPrefix"
[[menus.arguments]]
name = "addr6"
type = "ip6Addr"
[[menus.arguments]]
name = "prefix6"
type = "ip6Prefix"
[[menus.arguments]]
name = "rates"
type = "ubit (1Mbps, 2Mbps, 5.5Mbps, 11Mbps)"
[[menus.arguments]]
name = "truncated-ubit"
type = "ubit (mcs-0, mcs-1, mcs-2, mcs-3, mcs-4, mcs-5, mcs-6, mcs-7, mcs-8, mcs-9, mcs-10, mcs-11, mcs-12, mcs-13, m..."
[[menus.arguments]]
name = "untyped"
type = ""
[[menus.arguments]]
name = "maybe-unset"
type = "string"
unset = true
[[menus.arguments]]
name = "sticky"
type = "string"
[[menus.read_only]]
name = "actual"
type = "string"
"#,
    )
}

fn codes_for(data: &MenuData, doc: &str) -> Vec<Diagnostic> {
    compute_diagnostics(data, doc, "file:///validator-typed.rsc")
}

fn has_code(diags: &[Diagnostic], code: &str) -> bool {
    diags.iter().any(|d| d.code.as_deref() == Some(code))
}
// ── bool ────────────────────────────────────────────────────

#[test]
fn test_bool_invalid_is_hint() {
    let data = typed_data();
    let diags = codes_for(&data, "/demo/typed set flag=maybe");
    let d = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("invalid-bool-value"))
        .expect("invalid-bool-value must fire for 'maybe'");
    assert_eq!(d.severity, Some(severity::HINT));
    assert_eq!(d.source.as_deref(), Some("rsc-ls"));
    assert!(d.message.contains("maybe"));
    assert!(d.message.contains("flag"));
}

// ── num ─────────────────────────────────────────────────────

#[test]
fn test_num_allowlist_stays_silent() {
    let data = typed_data();
    for good in [
        "10", "1500", "0", "1.5", "10M", "100k", "0x10", "-5", "100%",
    ] {
        let doc = format!("/demo/typed set count={good}");
        let diags = codes_for(&data, &doc);
        assert!(
            !has_code(&diags, "invalid-num-value"),
            "num '{good}' must stay silent, got {diags:?}"
        );
    }
}

#[test]
fn test_num_invalid_is_hint() {
    let data = typed_data();
    let diags = codes_for(&data, "/demo/typed set count=abc");
    let d = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("invalid-num-value"))
        .expect("invalid-num-value must fire for 'abc'");
    assert_eq!(d.severity, Some(severity::HINT));
    assert_eq!(d.source.as_deref(), Some("rsc-ls"));
}

// ── time ────────────────────────────────────────────────────

#[test]
fn test_time_allowlist_stays_silent() {
    let data = typed_data();
    for good in [
        "00:10:00", "10:00", "1h30m", "30s", "10ms", "1d", "never", "infinite", "10",
    ] {
        let doc = format!("/demo/typed set period={good}");
        let diags = codes_for(&data, &doc);
        assert!(
            !has_code(&diags, "invalid-time-value"),
            "time '{good}' must stay silent, got {diags:?}"
        );
    }
}

#[test]
fn test_time_invalid_is_hint() {
    let data = typed_data();
    let diags = codes_for(&data, "/demo/typed set period=soonish");
    assert!(
        has_code(&diags, "invalid-time-value"),
        "time 'soonish' must hint, got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .filter(|d| d.code.as_deref() == Some("invalid-time-value"))
            .all(|d| d.severity == Some(severity::HINT))
    );
}

// ── macAddr ─────────────────────────────────────────────────

#[test]
fn test_mac_allowlist_stays_silent() {
    let data = typed_data();
    for good in [
        "AA:BB:CC:DD:EE:FF",
        "aa:bb:cc:dd:ee:ff",
        "01:23:45:67:89:ab",
    ] {
        let doc = format!("/demo/typed set mac={good}");
        let diags = codes_for(&data, &doc);
        assert!(
            !has_code(&diags, "invalid-mac-value"),
            "mac '{good}' must stay silent, got {diags:?}"
        );
    }
}

#[test]
fn test_mac_invalid_is_hint() {
    let data = typed_data();
    let diags = codes_for(&data, "/demo/typed set mac=not-a-mac");
    let d = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("invalid-mac-value"))
        .expect("invalid-mac-value must fire");
    assert_eq!(d.severity, Some(severity::HINT));
}

// ── ipAddr / ipPrefix family ────────────────────────────────
