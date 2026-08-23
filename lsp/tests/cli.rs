//! Integration tests for the `rsc-ls` CLI flags (`--version`, `--help`,
//! unknown-argument handling).
//!
//! These spawn the real binary via `CARGO_BIN_EXE_rsc-ls`, exercising the
//! same probe surface used to identify which copy of `rsc-ls` Zed resolved
//! at runtime (PATH → cache → GitHub download).
//!
//! Stdin is wired to `/dev/null`: a correct implementation never reads
//! stdin for these flags, and a broken one fails fast on EOF instead of
//! hanging CI.

use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_rsc-ls");

/// Run the binary with `args`, returning `(exit_code, stdout, stderr)`.
///
/// Panics if the child dies by signal (no exit code) — that itself would be
/// a CLI-contract violation worth failing on.
fn run(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(BIN)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("failed to spawn rsc-ls binary");
    let code = output
        .status
        .code()
        .expect("rsc-ls terminated by signal — CLI flags must exit normally");
    (
        code,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn version_flag_exits_zero_with_semver_on_stdout() {
    for flag in ["--version", "-V"] {
        let (code, stdout, stderr) = run(&[flag]);
        assert_eq!(code, 0, "{flag} must exit 0");
        assert!(
            stdout.starts_with("rsc-ls "),
            "{flag}: identity line must start with 'rsc-ls ', got {stdout:?}"
        );
        // Second whitespace token is the bare semver of this package.
        let version = stdout.split_whitespace().nth(1).unwrap_or_default();
        assert_eq!(
            version,
            env!("CARGO_PKG_VERSION"),
            "{flag}: must report CARGO_PKG_VERSION"
        );
        // Exactly one line — script-friendly output.
        assert_eq!(
            stdout.lines().count(),
            1,
            "{flag}: exactly one stdout line expected, got {stdout:?}"
        );
        assert!(
            stderr.is_empty(),
            "{flag}: stderr should stay clean, got {stderr:?}"
        );
    }
}

#[test]
fn help_flag_exits_zero_and_mentions_usage_on_stdout() {
    for flag in ["--help", "-h"] {
        let (code, stdout, stderr) = run(&[flag]);
        assert_eq!(code, 0, "{flag} must exit 0");
        assert!(
            stdout.to_ascii_lowercase().contains("usage"),
            "{flag}: stdout must mention usage, got {stdout:?}"
        );
        assert!(stderr.is_empty(), "{flag}: stderr should stay clean");
    }
}

#[test]
fn unknown_flag_exits_two_with_usage_on_stderr() {
    let (code, stdout, stderr) = run(&["--bogus"]);
    assert_eq!(code, 2, "unknown flag must exit 2");
    assert!(
        stdout.is_empty(),
        "errors go to stderr only, got {stdout:?}"
    );
    assert!(
        stderr.to_ascii_lowercase().contains("usage"),
        "stderr must include the usage text, got {stderr:?}"
    );
    assert!(
        stderr.contains("--bogus"),
        "stderr must name the offending argument, got {stderr:?}"
    );
}

#[test]
fn multiple_args_are_rejected_with_exit_two() {
    // Even recognizable flags have no defined combination semantics.
    let (code, _, stderr) = run(&["--version", "--help"]);
    assert_eq!(code, 2);
    assert!(
        stderr.to_ascii_lowercase().contains("usage"),
        "multi-arg rejection must still print usage, got {stderr:?}"
    );
}
