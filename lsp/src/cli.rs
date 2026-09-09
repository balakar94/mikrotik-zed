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
pub(crate) const USAGE: &str = "\
rsc-ls — MikroTik RouterOS Script language server (RouterOS 7.20+)

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
pub(crate) fn exit_code_for(command: &CliCommand) -> Option<i32> {
    match command {
        CliCommand::Serve => None,
        CliCommand::Version | CliCommand::Help => Some(0),
        CliCommand::UsageError(_) => Some(2),
    }
}

/// Render the full stderr blob for an invalid invocation: the reason first,
/// then a blank line and the shared usage text.
pub(crate) fn error_output(reason: &str) -> String {
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
pub(crate) fn build_sha_suffix(build_sha: Option<&str>) -> String {
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
