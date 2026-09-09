//! Diagnostics — unset and read-only.
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

// ── Unset guard: Hint-only ─────────────────────────────────────────

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
// ── ubit ────────────────────────────────────────────────────

#[test]
fn test_ubit_invalid_is_hint() {
    let data = typed_data();
    let diags = codes_for(&data, "/demo/typed set rates=bogus");
    let d = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("invalid-ubit-value"))
        .expect("invalid-ubit-value must fire for 'bogus'");
    assert_eq!(d.severity, Some(severity::HINT));
    assert!(d.message.contains("1Mbps"));
}

#[test]
fn test_ubit_truncated_and_untyped_stay_silent() {
    let data = typed_data();
    let diags = codes_for(&data, "/demo/typed set truncated-ubit=bogus");
    assert!(
        !has_code(&diags, "invalid-ubit-value"),
        "truncated ubit type must stay silent, got {diags:?}"
    );
    let diags = codes_for(&data, "/demo/typed set untyped=anything-at-all");
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref().is_some_and(|c| c.starts_with("invalid-"))),
        "empty type must stay silent, got {diags:?}"
    );
}

// ── dynamic values stay silent across families ──────────────

#[test]
fn test_dynamic_values_stay_silent() {
    let data = typed_data();
    for doc in [
        "/demo/typed set flag=$x",
        "/demo/typed set count=$n",
        "/demo/typed set period=$t",
        "/demo/typed set addr=$addr",
        "/demo/typed set rates=$r",
    ] {
        let diags = codes_for(&data, doc);
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref().is_some_and(|c| c.starts_with("invalid-"))),
            "'{doc}' uses a variable and must stay silent, got {diags:?}"
        );
    }
}

// ── Rule 10: unset adoption ─────────────────────────────────

#[test]
fn test_unset_verb_is_known() {
    let data = typed_data();
    let diags = codes_for(&data, "/demo/typed unset 0 maybe-unset");
    assert!(
        !has_code(&diags, "unknown-command"),
        "'unset' is a real RouterOS verb, got {diags:?}"
    );
    assert!(
        !has_code(&diags, "non-unsettable-property"),
        "unsettable property must stay silent, got {diags:?}"
    );
}

#[test]
fn test_unset_non_unsettable_is_hint_with_mark() {
    let data = typed_data();
    let diags = codes_for(&data, "/demo/typed unset 0 sticky");
    let d = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("non-unsettable-property"))
        .expect("non-unsettable-property must fire for 'sticky'");
    assert_eq!(d.severity, Some(severity::HINT));
    assert_eq!(d.source.as_deref(), Some("rsc-ls"));
    assert!(d.message.contains("sticky"), "got {:?}", d.message);
    assert!(
        d.message.contains("unsettable: no"),
        "message must carry the unsettable mark, got {:?}",
        d.message
    );
}

#[test]
fn test_unset_named_form_value_name() {
    let data = typed_data();
    let diags = codes_for(&data, "/demo/typed unset 0 value-name=sticky");
    assert!(
        has_code(&diags, "non-unsettable-property"),
        "named unset form must warn, got {diags:?}"
    );
    let ok = codes_for(&data, "/demo/typed unset 0 value-name=maybe-unset");
    assert!(
        !has_code(&ok, "non-unsettable-property"),
        "named unset of unsettable property must stay silent, got {ok:?}"
    );
}

#[test]
fn test_unset_unknown_names_stay_silent() {
    let data = typed_data();
    // Selectors and foreign names (interface names, numbers) are not
    // validated against the unset table.
    let diags = codes_for(&data, "/demo/typed unset 0 ether1");
    assert!(
        !has_code(&diags, "non-unsettable-property"),
        "unknown unset targets must stay silent, got {diags:?}"
    );
}

// ── Rule 11: read-only write ────────────────────────────────
