//! Platform naming for auto-downloaded language-server binaries.
//!
//! Two names are derived from `zed::current_platform()`:
//!
//! * [`asset_triple`] — the `<triple>` in the GitHub release asset name
//!   (`rsc-ls-<triple>`). Assets are extension-less byte blobs so one scheme
//!   covers every platform.
//! * [`server_binary_name`] — the local file/spawn name inside the extension
//!   work dir. Windows cannot spawn an executable image whose file lacks the
//!   `.exe` suffix, so downloads there are stored, cached and spawned as
//!   `rsc-ls.exe`; every other platform keeps the plain name.

use zed_extension_api::{self as zed};

use crate::{BINARY_NAME, GITHUB_REPO};

/// Maps `(os, arch)` to the release-asset triple, or explains why a platform
/// has no published binary (with manual-install instructions).
pub(crate) fn asset_triple(
    os: zed::Os,
    arch: zed::Architecture,
) -> std::result::Result<String, String> {
    use zed::Architecture as A;
    use zed::Os as O;
    match (os, arch) {
        (O::Mac, A::Aarch64) => Ok("aarch64-apple-darwin".to_string()),
        (O::Mac, A::X8664) => Ok("x86_64-apple-darwin".to_string()),
        (O::Mac, A::X86) => Ok("x86_64-apple-darwin".to_string()),
        (O::Linux, A::Aarch64) => Ok("aarch64-unknown-linux-gnu".to_string()),
        (O::Linux, A::X8664) => Ok("x86_64-unknown-linux-gnu".to_string()),
        (O::Linux, A::X86) => Ok("x86_64-unknown-linux-gnu".to_string()),
        (O::Windows, A::X8664) => Ok("x86_64-pc-windows-msvc".to_string()),
        (O::Windows, A::Aarch64) => Ok("aarch64-pc-windows-msvc".to_string()),
        (os, arch) => Err(format!(
            "Platform not supported for {BINARY_NAME} auto-download (os={os:?} arch={arch:?}). \
            Install {BINARY_NAME} manually: cargo build -p rsc-ls --release and put it in PATH, \
            or download a binary from https://github.com/{GITHUB_REPO}/releases"
        )),
    }
}

/// Local file/spawn name of the language-server binary for a platform
/// (`rsc-ls.exe` on Windows, plain [`BINARY_NAME`] elsewhere).
pub(crate) fn server_binary_name(os: zed::Os) -> &'static str {
    match os {
        zed::Os::Windows => "rsc-ls.exe",
        _ => BINARY_NAME,
    }
}

/// True when `path` exists and is spawnable as-is.
///
/// On unix that means the file carries at least one exec bit; on Windows any
/// existing readable file is spawnable, so existence suffices. Callers use
/// this instead of `zed::make_file_executable().is_ok()`, which the host turns
/// into an unconditional no-op-Ok on non-unix platforms (it says nothing about
/// presence), and instead of bare `fs::metadata` when a lost exec bit should
/// trigger self-healing via re-download rather than a doomed spawn.
pub(crate) fn is_executable(path: &str) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(path).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_triple_covers_all_published_binaries() {
        let cases = [
            (
                zed::Os::Mac,
                zed::Architecture::Aarch64,
                "aarch64-apple-darwin",
            ),
            (
                zed::Os::Mac,
                zed::Architecture::X8664,
                "x86_64-apple-darwin",
            ),
            (
                zed::Os::Linux,
                zed::Architecture::Aarch64,
                "aarch64-unknown-linux-gnu",
            ),
            (
                zed::Os::Linux,
                zed::Architecture::X8664,
                "x86_64-unknown-linux-gnu",
            ),
            (
                zed::Os::Windows,
                zed::Architecture::X8664,
                "x86_64-pc-windows-msvc",
            ),
            (
                zed::Os::Windows,
                zed::Architecture::Aarch64,
                "aarch64-pc-windows-msvc",
            ),
        ];
        for (os, arch, triple) in cases {
            assert_eq!(asset_triple(os, arch).unwrap(), triple);
        }
    }

    #[test]
    fn windows_spawn_name_carries_exe_suffix() {
        assert_eq!(server_binary_name(zed::Os::Windows), "rsc-ls.exe");
    }

    #[test]
    fn unix_spawn_names_stay_plain() {
        assert_eq!(server_binary_name(zed::Os::Mac), BINARY_NAME);
        assert_eq!(server_binary_name(zed::Os::Linux), BINARY_NAME);
    }

    #[test]
    fn missing_files_are_not_executable() {
        assert!(!is_executable("definitely-missing-rsc-ls-binary"));
    }

    #[cfg(unix)]
    #[test]
    fn is_executable_tracks_the_unix_exec_bit() {
        use std::os::unix::fs::PermissionsExt;

        let mut path = std::env::temp_dir();
        path.push("rsc-zed-is-executable-probe");
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();

        // 0644 — present but not spawnable.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!is_executable(path.to_str().unwrap()));

        // 0755 — the exact mode `make_file_executable` sets on unix.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_executable(path.to_str().unwrap()));

        let _ = std::fs::remove_file(&path);
    }
}
