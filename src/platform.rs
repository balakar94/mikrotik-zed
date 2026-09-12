//! Platform naming for auto-downloaded language-server binaries.
//!
//! Three names derive from `zed::current_platform()`:
//!
//! * [`asset_triple`] — the `<triple>` in the GitHub release asset name
//!   (`rsc-ls-<triple>`). Assets are extension-less byte blobs so one scheme
//!   covers every platform.
//! * [`server_binary_name`] — the unversioned name used to probe PATH (a
//!   developer build or manual install carries no version suffix). Windows
//!   cannot spawn an executable image whose file lacks the `.exe` suffix, so
//!   the Windows probe name is `rsc-ls.exe`.
//! * [`stored_binary_name`] — the versioned work-dir storage name for
//!   downloaded binaries. Zed keys the extension work dir by manifest id, not
//!   by extension version, so the version must live in the filename itself;
//!   otherwise an updated extension would keep finding and reusing a stale
//!   binary forever.
//!
//! Honesty note on [`is_executable`]: in the shipped wasm32-wasip2 component
//! `cfg(unix)` is false, so it reduces to an existence check on every host
//! OS. Integrity of cached binaries is enforced by the [`crate::cache`]
//! digest-marker gate instead; exec bits only matter in native test builds.

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
/// (`rsc-ls.exe` on Windows, plain [`BINARY_NAME`] elsewhere). Used only for
/// PATH probing; work-dir storage uses [`stored_binary_name`].
pub(crate) fn server_binary_name(os: zed::Os) -> &'static str {
    match os {
        zed::Os::Windows => "rsc-ls.exe",
        _ => BINARY_NAME,
    }
}

/// Versioned work-dir storage name for a downloaded binary
/// (`rsc-ls-<version>.exe` on Windows, `rsc-ls-<version>` elsewhere).
///
/// Zed keys the extension work dir by manifest id only, so the version must
/// be part of the filename: with a fixed name, an extension update shipping a
/// newer language server would find and reuse the previous release's binary
/// silently forever.
pub(crate) fn stored_binary_name(os: zed::Os, version: &str) -> String {
    match os {
        zed::Os::Windows => format!("{BINARY_NAME}-{version}.exe"),
        _ => format!("{BINARY_NAME}-{version}"),
    }
}

/// Builds the canonical release-download URL for `asset_name` under `tag`,
/// pinned to this extension's own repository.
///
/// Used instead of any API-supplied URL whenever the tag is already known
/// (the `github_release_by_tag_name` path), so a tampered or stale API
/// response cannot substitute a different host, path, or asset.
pub(crate) fn pinned_asset_url(tag: &str, asset_name: &str) -> String {
    format!("https://github.com/{GITHUB_REPO}/releases/download/{tag}/{asset_name}")
}

/// Accepts a GitHub-API asset `download_url` only when it is exactly this
/// extension's own release-download URL for `asset_name`:
/// `https://github.com/{GITHUB_REPO}/releases/download/<tag>/<asset_name>`.
///
/// Anything else — foreign scheme/host/repo, a query string or fragment, a
/// backslash, whitespace, an extra path segment, a different asset — yields
/// `None` so the caller falls back to a URL it constructs itself from
/// [`GITHUB_REPO`]. Defense in depth: even when `asset.name` matched, a
/// tampered API response cannot smuggle a different path into the download
/// (which would then also have to supply a matching `.sha256` companion in
/// this repo's namespace).
///
/// `<tag>` must be non-empty and slash-free (this project's release tags are
/// validated as `vMAJOR.MINOR.PATCH`), so the returned URL has exactly one
/// path segment before the asset name.
pub(crate) fn pinned_release_url(candidate: &str, asset_name: &str) -> Option<String> {
    let prefix = format!("https://github.com/{GITHUB_REPO}/releases/download/");
    let rest = candidate.strip_prefix(&prefix)?;
    if rest.bytes().any(|b| {
        b == b'?' || b == b'#' || b == b'\\' || b.is_ascii_whitespace() || b.is_ascii_control()
    }) {
        return None;
    }
    let (tag, base) = rest.split_once('/')?;
    if tag.is_empty() || tag.contains('/') || base != asset_name {
        return None;
    }
    Some(candidate.to_string())
}

/// True when `path` exists and is spawnable as-is.
///
/// Honest semantics: in the shipped wasm32-wasip2 component `cfg(unix)` is
/// false, so this reduces to an existence check on every host OS; integrity
/// is enforced by the [`crate::cache`] digest-marker gate, and exec bits only
/// matter in native test builds (where the unix arm below does check them).
/// Callers use this instead of `zed::make_file_executable().is_ok()`, which
/// the host turns into an unconditional no-op-Ok on non-unix platforms (it
/// says nothing about presence), and instead of bare `fs::metadata` when a
/// lost exec bit should trigger re-download rather than a doomed spawn in
/// native builds.
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

    #[test]
    fn stored_names_are_version_keyed_per_platform() {
        assert_eq!(
            stored_binary_name(zed::Os::Windows, "0.5.0"),
            "rsc-ls-0.5.0.exe"
        );
        assert_eq!(stored_binary_name(zed::Os::Mac, "0.5.0"), "rsc-ls-0.5.0");
        assert_eq!(stored_binary_name(zed::Os::Linux, "0.5.0"), "rsc-ls-0.5.0");
    }

    #[test]
    fn stored_names_never_collide_across_versions() {
        // The whole point of version keying: an updated extension must not
        // find (and silently reuse) the previous release's binary.
        assert_ne!(
            stored_binary_name(zed::Os::Mac, "0.4.0"),
            stored_binary_name(zed::Os::Mac, "0.5.0")
        );
        assert_ne!(
            stored_binary_name(zed::Os::Windows, "0.4.0"),
            stored_binary_name(zed::Os::Windows, "0.5.0")
        );
    }

    #[test]
    fn release_urls_are_pinned_to_this_repo_namespace() {
        let asset = "rsc-ls-x86_64-unknown-linux-gnu";
        let good = format!("https://github.com/{GITHUB_REPO}/releases/download/v0.5.0/{asset}");
        assert_eq!(pinned_release_url(&good, asset), Some(good.clone()));

        // Foreign repo under the same host.
        assert_eq!(
            pinned_release_url(
                "https://github.com/attacker/mikrotik-zed/releases/download/v0.5.0/rsc-ls",
                "rsc-ls"
            ),
            None
        );
        // Scheme downgrade.
        assert_eq!(
            pinned_release_url(
                "http://github.com/balakar94/mikrotik-zed/releases/download/v0.5.0/rsc-ls",
                "rsc-ls"
            ),
            None
        );
        // Prefix look-alike: the repo name is a prefix of a different path.
        assert_eq!(
            pinned_release_url(
                "https://github.com/balakar94/mikrotik-zed.evil/releases/download/v0.5.0/rsc-ls",
                "rsc-ls"
            ),
            None
        );
        // Host look-alike.
        assert_eq!(
            pinned_release_url(
                "https://github.com.evil.com/balakar94/mikrotik-zed/releases/download/v0.5.0/rsc-ls",
                "rsc-ls"
            ),
            None
        );
        assert_eq!(pinned_release_url("", asset), None);
    }

    #[test]
    fn pinned_release_url_rejects_urls_that_are_not_the_exact_asset() {
        let asset = "rsc-ls-x86_64-unknown-linux-gnu";
        let prefix = format!("https://github.com/{GITHUB_REPO}/releases/download/");
        let cases = [
            // Prefix with no tag/asset at all.
            prefix.clone(),
            // Missing asset segment.
            format!("{prefix}v0.5.0"),
            // Asset name mismatch (same namespace, wrong file).
            format!("{prefix}v0.5.0/rsc-ls-aarch64-apple-darwin"),
            // Extra path segment smuggled into the tag position.
            format!("{prefix}extra/v0.5.0/{asset}"),
            // Query string / fragment would change what the companion URL
            // resolves to.
            format!("{prefix}v0.5.0/{asset}?raw=1"),
            format!("{prefix}v0.5.0/{asset}#frag"),
            // Backslash could be normalized to a separator by some URL stacks.
            format!("{prefix}v0.5.0\\{asset}"),
            // Whitespace/control characters must never appear in a URL.
            format!("{prefix}v0.5.0/{asset} "),
            format!("{prefix}v0.5.0/{asset}\n"),
        ];
        for candidate in cases {
            assert_eq!(
                pinned_release_url(&candidate, asset),
                None,
                "must reject {candidate:?}"
            );
        }
    }

    #[test]
    fn pinned_asset_url_builds_the_canonical_download_url() {
        assert_eq!(
            pinned_asset_url("v0.5.0", "rsc-ls-x86_64-unknown-linux-gnu"),
            format!(
                "https://github.com/{GITHUB_REPO}/releases/download/v0.5.0/rsc-ls-x86_64-unknown-linux-gnu"
            )
        );
    }
}
