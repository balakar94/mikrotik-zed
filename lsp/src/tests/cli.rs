//! White-box: cli (cli).
use crate::cli::*;

// ── version_string ────────────────────────────────────────────

#[test]
fn test_version_string_contains_cargo_pkg_version() {
    let v = version_string();
    assert_eq!(
        v.split(' ').next(),
        Some("rsc-ls"),
        "identity line must start with the binary name"
    );
    // Second token is the bare semver regardless of which build-sha
    // branch this compilation took.
    assert_eq!(
        v.split(' ').nth(1),
        Some(env!("CARGO_PKG_VERSION")),
        "identity line must carry CARGO_PKG_VERSION"
    );
}

#[test]
fn test_version_string_is_single_plain_line() {
    let v = version_string();
    assert!(!v.contains('\n'), "must be one script-friendly line");
    assert_eq!(v, v.trim_end(), "no trailing whitespace");
}

// ── build_sha_suffix (both branches, pure) ────────────────────

#[test]
fn test_build_sha_suffix_absent_yields_empty() {
    assert_eq!(build_sha_suffix(None), "");
}

#[test]
fn test_build_sha_suffix_blank_yields_empty() {
    assert_eq!(build_sha_suffix(Some("")), "");
    assert_eq!(build_sha_suffix(Some("   ")), "");
}

#[test]
fn test_build_sha_suffix_takes_first_seven_chars() {
    assert_eq!(
        build_sha_suffix(Some("bbfadd03ddc9599b85f8d684d62ebe06c822b78d")),
        " (build bbfadd0)"
    );
}

#[test]
fn test_build_sha_suffix_short_input_used_whole() {
    assert_eq!(build_sha_suffix(Some("abc")), " (build abc)");
}

// ── parse_cli_args ────────────────────────────────────────────

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn test_parse_no_args_serves() {
    assert_eq!(parse_cli_args(&args(&[])), CliCommand::Serve);
}

#[test]
fn test_parse_version_flags() {
    assert_eq!(parse_cli_args(&args(&["--version"])), CliCommand::Version);
    assert_eq!(parse_cli_args(&args(&["-V"])), CliCommand::Version);
}

#[test]
fn test_parse_help_flags() {
    assert_eq!(parse_cli_args(&args(&["--help"])), CliCommand::Help);
    assert_eq!(parse_cli_args(&args(&["-h"])), CliCommand::Help);
}

#[test]
fn test_parse_unknown_flag_is_usage_error_naming_it() {
    let parsed = parse_cli_args(&args(&["--bogus"]));
    match parsed {
        CliCommand::UsageError(reason) => assert!(
            reason.contains("--bogus"),
            "reason must name the offending argument, got: {reason}"
        ),
        other => panic!("expected UsageError, got {other:?}"),
    }
}

#[test]
fn test_parse_multiple_args_rejected_even_if_recognizable() {
    // Combinations have no defined meaning; never silently pick one.
    let parsed = parse_cli_args(&args(&["--version", "--help"]));
    assert!(
        matches!(parsed, CliCommand::UsageError(_)),
        "multiple arguments must be a usage error, got {parsed:?}"
    );
}

#[test]
fn test_usage_error_reason_mentions_count_for_multi_arg() {
    let parsed = parse_cli_args(&args(&["a", "b", "c"]));
    match parsed {
        CliCommand::UsageError(reason) => {
            assert!(reason.contains('3'), "reason should report arity: {reason}")
        }
        other => panic!("expected UsageError, got {other:?}"),
    }
}

// ── exit_code_for / error_output (pure, no real streams) ──────

#[test]
fn test_exit_code_matrix() {
    assert_eq!(exit_code_for(&CliCommand::Serve), None);
    assert_eq!(exit_code_for(&CliCommand::Version), Some(0));
    assert_eq!(exit_code_for(&CliCommand::Help), Some(0));
    assert_eq!(
        exit_code_for(&CliCommand::UsageError("--x".to_string())),
        Some(2)
    );
}

#[test]
fn test_error_output_leads_with_reason_and_usage() {
    let out = error_output("unrecognized argument '--bogus'");
    assert!(out.starts_with("error: unrecognized argument '--bogus'\n"));
    assert!(out.to_ascii_lowercase().contains("usage"));
    // Same shared text `--help` prints on stdout, so both paths can
    // never drift apart.
    assert!(out.contains(USAGE));
}
