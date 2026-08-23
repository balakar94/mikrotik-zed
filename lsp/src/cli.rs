// ── Binary observability: CLI flags and version identity ─────────
//
// The Zed extension resolves `rsc-ls` at runtime (PATH → cached extension
// dir → GitHub download), so several builds can coexist on one machine and
// a stale copy can hide behind a fresh one. These flags give every copy a
// way to answer "who are you?" without touching stdin or paying for menu
// loading.
//
// Contract (deliberately boring, script-friendly):
//   no args       → serve LSP over stdio (unchanged default behavior)
//   -V/--version  → identity line on stdout, exit 0
//   -h/--help     → usage on stdout, exit 0
//   anything else → reason + usage on stderr, exit 2

use std::io::Write;

/// Length of the shortened commit SHA appended by [`build_sha_suffix`]
/// (the conventional `git rev-parse --short=7` width).
const SHA_SHORT_LEN: usize = 7;

/// Usage text shared by `--help` (printed to stdout) and argument errors
/// (printed to stderr after the offending reason).
const USAGE: &str = "\
rsc-ls — MikroTik RouterOS Script language server (RouterOS 7.0+)

Usage:
  rsc-ls             Serve LSP over stdio (default; this is how Zed starts it)
  rsc-ls --version   Print the version line to stdout and exit
  rsc-ls --help      Show this help and exit

Environment:
  RSC_LS_LOG=<error|warn|info|debug|trace>   Stderr log verbosity (default: info)
";

/// Outcome of inspecting the process arguments.
#[derive(Debug, PartialEq)]
pub(crate) enum CliCommand {
    /// No arguments — continue into the normal LSP-over-stdio path.
    Serve,
    /// `--version` / `-V`: print [`version_string`] to stdout, exit 0.
    Version,
    /// `--help` / `-h`: print [`USAGE`] to stdout, exit 0.
    Help,
    /// Anything else: print reason + [`USAGE`] to stderr, exit 2.
    UsageError(String),
}

/// Parse process arguments (already stripped of `argv[0]`) into a command.
///
/// Pure so the full flag matrix is unit-testable without spawning processes;
/// `tests/cli.rs` additionally covers the real binary end-to-end.
pub(crate) fn parse_cli_args(args: &[String]) -> CliCommand {
    match args {
        [] => CliCommand::Serve,
        [only] => match only.as_str() {
            "--version" | "-V" => CliCommand::Version,
            "--help" | "-h" => CliCommand::Help,
            other => CliCommand::UsageError(format!("unrecognized argument '{other}'")),
        },
        // Exactly one flag is accepted; combinations have no defined meaning,
        // so fail loudly instead of silently picking a winner.
        _ => CliCommand::UsageError(format!(
            "expected at most one argument, got {}: {args:?}",
            args.len()
        )),
    }
}

/// Execute a parsed CLI command.
///
/// Returns the process exit code for terminal commands, or `None` when the
/// caller must continue into the LSP serve path. Output uses explicit
/// best-effort writes: when stdout is already closed (broken pipe), a
/// diagnostic cannot be delivered and panicking would misreport a healthy
/// binary — the documented exit codes still stand.
///
/// Rendering ([`error_output`], [`USAGE`], [`version_string`]) is separated
/// from emission so unit tests never touch real streams; `tests/cli.rs`
/// covers the actual binary's stdout/stderr end-to-end.
pub(crate) fn run_cli_command(command: CliCommand) -> Option<i32> {
    match &command {
        CliCommand::Serve => {}
        CliCommand::Version => emit_stdout(&format!("{}\n", version_string())),
        CliCommand::Help => emit_stdout(USAGE),
        CliCommand::UsageError(reason) => {
            let mut stderr = std::io::stderr().lock();
            let _ = stderr.write_all(error_output(reason).as_bytes());
            let _ = stderr.flush();
        }
    }
    exit_code_for(&command)
}

/// Process exit code per [`CliCommand`] (`None` = keep serving).
///
/// Pure counterpart of [`run_cli_command`] so the code matrix is
/// assertable without performing any I/O.
fn exit_code_for(command: &CliCommand) -> Option<i32> {
    match command {
        CliCommand::Serve => None,
        CliCommand::Version | CliCommand::Help => Some(0),
        CliCommand::UsageError(_) => Some(2),
    }
}

/// Render the full stderr blob for an invalid invocation: the reason first,
/// then a blank line and the shared usage text.
fn error_output(reason: &str) -> String {
    format!("error: {reason}\n\n{USAGE}")
}

/// Plain, script-friendly identity line for this binary: `rsc-ls <semver>`,
/// plus ` (build <short-sha>)` when release CI set `RSC_LS_BUILD_SHA` at
/// compile time. This is exactly what `--version` prints and what the
/// startup stderr banner embeds, so copies seen in logs can be matched to
/// binaries on disk unambiguously.
pub(crate) fn version_string() -> String {
    format!(
        "rsc-ls {}{}",
        env!("CARGO_PKG_VERSION"),
        build_sha_suffix(option_env!("RSC_LS_BUILD_SHA"))
    )
}

/// Pure suffix decision behind [`version_string`], factored out so both
/// branches are unit-testable (`option_env!` is frozen at compile time and
/// cannot be flipped at runtime).
///
/// `Some(<sha>)` yields ` (build <first 7 chars>)`; `None`, empty, or
/// whitespace-only input yields no suffix. Inputs shorter than 7 characters
/// are used whole instead of failing — the value is diagnostic metadata,
/// not a validated identifier.
fn build_sha_suffix(build_sha: Option<&str>) -> String {
    match build_sha.map(str::trim).filter(|sha| !sha.is_empty()) {
        Some(sha) => {
            let short: String = sha.chars().take(SHA_SHORT_LEN).collect();
            format!(" (build {short})")
        }
        None => String::new(),
    }
}

fn emit_stdout(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── version_string ────────────────────────────────────────────

    #[test]
    fn test_version_string_contains_cargo_pkg_version() {
        let v = version_string();
        assert_eq!(
            v.split(' ').nth(0),
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
}
