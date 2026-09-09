// Diagnostics — unset and read-only.
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

// ── Unset guard: Hint-only ───────────────────────────────────────────────

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
// ── ubit ─────────────────────────────────────────────────────────────────

#[test]
fn test_read_only_write_is_info() {
    let data = typed_data();
    let diags = codes_for(&data, "/demo/typed add actual=x");
    let d = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("read-only-write"))
        .expect("read-only-write must fire for read_only column on add");
    assert_eq!(d.severity, Some(severity::INFORMATION));
    assert_eq!(d.source.as_deref(), Some("rsc-ls"));
    assert!(d.message.contains("actual"), "got {:?}", d.message);
    assert!(d.message.contains("read-only"), "got {:?}", d.message);
}

#[test]
fn test_read_only_write_not_for_other_verbs_or_keys() {
    let data = typed_data();
    let diags = codes_for(&data, "/demo/typed add flag=yes");
    assert!(
        !has_code(&diags, "read-only-write"),
        "writable property must stay silent, got {diags:?}"
    );
    // `print` never writes: no diagnostic even for read-only names.
    let diags = codes_for(&data, "/demo/typed print");
    assert!(!has_code(&diags, "read-only-write"), "got {diags:?}");
}
