// Diagnostics — typed validators.
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

// ── Typed validators: Hint-only ──────────────────────────────────────────

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
// ── bool ─────────────────────────────────────────────────────────────────

#[test]
fn validator_typed_bool_invalid_is_hint_only() {
    let data = validator_data();
    // Invalid bool value: no Warning/Error may fire; the
    // `invalid-bool-value` diagnostic must be Hint.
    let diags = crate::diagnostics::compute_diagnostics(
        &data,
        "/validator/typed add flag=maybe count=1",
        "file:///validator-typed-bool.rsc",
    );
    assert_validator_severity_contract(&diags);
    // Presence pin (M4): deleting the bool validator must fail this test.
    let hints: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_deref() == Some("invalid-bool-value"))
        .collect();
    assert_eq!(hints.len(), 1, "invalid bool must hint once, got {diags:?}");
    assert_eq!(hints[0].severity, Some(severity::HINT));
    assert_eq!(hints[0].source.as_deref(), Some("rsc-ls"));
    assert!(
        hints[0].message.contains("flag") && hints[0].message.contains("maybe"),
        "hint must name property and value, got {:?}",
        hints[0].message,
    );
}

#[test]
fn validator_typed_scalar_family_invalid_is_hint_only() {
    let data = validator_data();
    // One invalid value per scalar family; loop keeps the pin tight
    // without new dependencies.
    let cases = [
        ("count", "notanum"),
        ("uptime", "notatime"),
        ("hw", "zz:zz:zz:zz:zz:zz"),
        ("peer", "999.999.999.999"),
        ("bits", "notabits!!"),
    ];
    let codes = [
        "invalid-num-value",
        "invalid-time-value",
        "invalid-mac-value",
        "invalid-ip-value",
        "invalid-ubit-value",
    ];
    for ((prop, bad), code) in cases.iter().zip(codes) {
        let doc = format!("/validator/typed add flag=yes {prop}={bad}");
        let diags = crate::diagnostics::compute_diagnostics(
            &data,
            &doc,
            "file:///validator-typed-scalar.rsc",
        );
        assert_validator_severity_contract(&diags);
        assert!(
            !diags
                .iter()
                .any(|d| d.severity == Some(severity::WARNING) && d.message.contains(prop)),
            "invalid {prop}={bad} must not warn, got {diags:?}",
        );
        // Presence pin (M4): deleting the {prop} validator must fail.
        let hits: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_deref() == Some(code))
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "invalid {prop}={bad} must surface one `{code}` Hint, got {diags:?}",
        );
        assert_eq!(hits[0].severity, Some(severity::HINT));
        assert_eq!(hits[0].source.as_deref(), Some("rsc-ls"));
    }
    // Valid values stay fully silent (no false-positive guard rail).
    let diags = crate::diagnostics::compute_diagnostics(
        &data,
        "/validator/typed add flag=yes count=5 uptime=1h hw=AA:BB:CC:DD:EE:FF peer=10.0.0.1 bits=1Mbps",
        "file:///validator-typed-valid.rsc",
    );
    assert!(
        diags.is_empty(),
        "valid scalar values must stay silent, got {diags:?}",
    );
}

#[test]
fn validator_typed_empty_type_stays_silent() {
    let data = validator_data();
    // Empty-type properties accept anything: garbage values stay silent.
    let diags = crate::diagnostics::compute_diagnostics(
        &data,
        "/validator/typed add flag=yes freeform=anything-at-all!!!",
        "file:///validator-typed-empty.rsc",
    );
    assert!(
        diags.iter().all(|d| d.code.as_deref() != Some("truncated")),
        "no truncation expected on a two-property doc, got {diags:?}",
    );
    assert!(
        !diags.iter().any(|d| d.message.contains("freeform")),
        "empty-type property must stay silent, got {diags:?}",
    );
}

#[test]
fn test_bool_allowlist_stays_silent() {
    let data = typed_data();
    for good in ["yes", "no", "true", "false", "on", "off", "YES", "No", "ON"] {
        let doc = format!("/demo/typed set flag={good}");
        let diags = codes_for(&data, &doc);
        assert!(
            !has_code(&diags, "invalid-bool-value"),
            "bool '{good}' must stay silent, got {diags:?}"
        );
    }
}
