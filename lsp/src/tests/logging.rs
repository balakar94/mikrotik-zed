//! White-box: logging (logging).
use crate::logging::*;

#[test]
fn utc_epoch_zero_formats_as_1970() {
    assert_eq!(format_utc_rfc3339(0), "1970-01-01T00:00:00Z");
}

#[test]
fn utc_day_10957_formats_as_y2k() {
    // 1970-01-01 → 2000-01-01 is 30 years + 7 leap days = 10957 days.
    assert_eq!(format_utc_rfc3339(10_957 * 86_400), "2000-01-01T00:00:00Z");
}

#[test]
fn utc_time_of_day_rolls_over_midnight() {
    assert_eq!(format_utc_rfc3339(86_400 + 3661), "1970-01-02T01:01:01Z");
}

#[test]
fn level_tokens_are_lowercase() {
    assert_eq!(LogLevel::Error.as_str(), "error");
    assert_eq!(LogLevel::Warn.as_str(), "warn");
    assert_eq!(LogLevel::Info.as_str(), "info");
    assert_eq!(LogLevel::Debug.as_str(), "debug");
    assert_eq!(LogLevel::Trace.as_str(), "trace");
}

#[test]
fn elapsed_tag_shape_is_stable() {
    let tag = elapsed_tag();
    assert!(tag.starts_with("[T+"), "got {tag}");
    assert!(tag.ends_with("s]"), "got {tag}");
}

#[test]
fn sanitize_strips_newlines_and_truncates() {
    assert_eq!(sanitize_for_log("a\nb\rc"), "abc");
    assert!(!sanitize_for_log("x\ny").contains('\n'));
    assert!(!sanitize_for_log("x\ry").contains('\r'));
    let long = "a".repeat(200);
    assert_eq!(sanitize_for_log(&long).len(), 128);
    assert_eq!(sanitize_for_log("router.local"), "router.local");
}

#[test]
fn redact_secrets_covers_password_and_base64() {
    let pass = "s3cret-p@ss";
    let user = "admin";
    use base64::Engine;
    let basic =
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}").as_bytes());
    let pass_b64 = base64::engine::general_purpose::STANDARD.encode(pass.as_bytes());
    let text = format!("failed with {pass} basic={basic} alone={pass_b64} user={user}");
    let out = redact_secrets(&text, pass, user);
    assert!(!out.contains(pass), "password leaked: {out}");
    assert!(!out.contains(&basic), "basic leaked: {out}");
    assert!(!out.contains(&pass_b64), "pass b64 leaked: {out}");
    assert!(out.contains("[REDACTED]"));
    // Username alone is not a secret and stays visible.
    assert!(out.contains(user));
    // Empty pass is a no-op.
    assert_eq!(redact_secrets("hello", "", user), "hello");
}

#[test]
fn truncate_command_caps_at_256_and_strips_newlines() {
    let long = "c".repeat(300);
    assert_eq!(truncate_command_for_log(&long).chars().count(), 256);
    assert_eq!(truncate_command_for_log("a\nb\rc"), "abc");
}
