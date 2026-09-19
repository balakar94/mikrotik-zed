//! Auto-download release selection: tag naming, asset matching, source order.
//!
//! The host API calls (`github_release_by_tag_name`, `latest_github_release`)
//! stay in `lib.rs`; this module owns the naming and ordering rules so they
//! are unit-testable without network access.
//!
//! Resolution order (immutability first):
//!
//! 1. The release tagged `v{extension version}`. Its URL is built from pinned
//!    constants; API-supplied URLs are never trusted on this path.
//! 2. The latest stable release, only when the pinned release is absent or
//!    lacks this platform's asset. `pre_release: false` keeps prereleases out
//!    of the fallback.
//! 3. A direct download URL built from the same pinned constants.
//!
//! A prerelease extension version (`0.7.0-rc.1`) resolves its own exact tag
//! (`v0.7.0-rc.1`) because tag lookup matches the tag exactly; the
//! latest-stable fallback still filters prereleases.

/// Tag name for an extension version: `0.7.0` -> `v0.7.0`.
///
/// The version comes from `env!("CARGO_PKG_VERSION")` (compile-time, never
/// `std::env`). Prerelease and build suffixes are preserved verbatim so an RC
/// extension resolves its own RC release.
pub(crate) fn tag_for_version(version: &str) -> String {
    format!("v{version}")
}

/// Exact, case-sensitive match between a release asset name and the expected
/// `rsc-ls-<triple>` name.
///
/// Exactness is required: the `.sha256` companion URL is derived by appending
/// `.sha256` to the selected asset URL, and the strict companion parser
/// expects that same filename, so a prefix or case-insensitive match could
/// select a look-alike asset.
pub(crate) fn asset_matches(asset_name: &str, expected: &str) -> bool {
    asset_name == expected
}

/// Which lookup supplied the download URL.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// Release tagged `v{extension version}` (preferred).
    PinnedTag,
    /// Latest stable release (fallback).
    LatestStable,
    /// Direct URL built from pinned constants (final fallback).
    DirectUrl,
}

impl Source {
    /// Log label naming the chosen origin.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::PinnedTag => "pinned to extension version",
            Self::LatestStable => "latest stable fallback",
            Self::DirectUrl => "direct URL fallback",
        }
    }
}

/// Choose the download source from the two lookups' outcomes.
///
/// Order is immutability-first: the pinned tag wins whenever it carries the
/// asset, so a newer stable release can never replace the language server of
/// an already-published extension snapshot. The latest stable release is used
/// only when the pinned release is absent or lacks the asset, and only when
/// its API-supplied URL passed the pinned-repo check
/// (`latest_url_verified`). Anything else falls back to the direct URL.
pub(crate) fn choose_source(
    pinned_has_asset: bool,
    latest_has_asset: bool,
    latest_url_verified: bool,
) -> Source {
    if pinned_has_asset {
        Source::PinnedTag
    } else if latest_has_asset && latest_url_verified {
        Source::LatestStable
    } else {
        Source::DirectUrl
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_for_version_prefixes_v() {
        assert_eq!(tag_for_version("0.7.0"), "v0.7.0");
        assert_eq!(tag_for_version("1.2.3"), "v1.2.3");
    }

    #[test]
    fn tag_for_version_preserves_prerelease_and_build_suffixes() {
        // Exact tag lookup is what lets an RC extension resolve its own RC
        // release instead of a stable one.
        assert_eq!(tag_for_version("0.7.0-rc.1"), "v0.7.0-rc.1");
        assert_eq!(tag_for_version("0.7.0+build.5"), "v0.7.0+build.5");
    }

    #[test]
    fn asset_matching_is_exact_and_case_sensitive() {
        let expected = "rsc-ls-x86_64-unknown-linux-gnu";
        assert!(asset_matches(expected, expected));
        // Companion and other-platform assets must not match.
        assert!(!asset_matches(
            "rsc-ls-x86_64-unknown-linux-gnu.sha256",
            expected
        ));
        assert!(!asset_matches("rsc-ls-aarch64-unknown-linux-gnu", expected));
        // Case differences are not equivalent.
        assert!(!asset_matches("RSC-LS-X86_64-UNKNOWN-LINUX-GNU", expected));
        // Prefix/suffix look-alikes.
        assert!(!asset_matches(
            "rsc-ls-x86_64-unknown-linux-gnu.bak",
            expected
        ));
        assert!(!asset_matches(
            "x-rsc-ls-x86_64-unknown-linux-gnu",
            expected
        ));
        assert!(!asset_matches("", expected));
    }

    #[test]
    fn pinned_release_wins_over_latest_stable() {
        // The immutability property: a newer stable release must never
        // replace the binary shipped by an older extension snapshot.
        assert_eq!(choose_source(true, true, true), Source::PinnedTag);
        assert_eq!(choose_source(true, true, false), Source::PinnedTag);
        assert_eq!(choose_source(true, false, false), Source::PinnedTag);
    }

    #[test]
    fn latest_stable_used_only_when_pinned_lacks_the_asset() {
        assert_eq!(choose_source(false, true, true), Source::LatestStable);
        // Asset present but the API URL failed the pinned-repo check: no
        // trusted URL, so fall through to the direct URL.
        assert_eq!(choose_source(false, true, false), Source::DirectUrl);
    }

    #[test]
    fn direct_url_when_neither_lookup_is_usable() {
        assert_eq!(choose_source(false, false, false), Source::DirectUrl);
        assert_eq!(choose_source(false, false, true), Source::DirectUrl);
    }

    #[test]
    fn source_labels_name_the_origin() {
        assert_eq!(Source::PinnedTag.label(), "pinned to extension version");
        assert_eq!(Source::LatestStable.label(), "latest stable fallback");
        assert_eq!(Source::DirectUrl.label(), "direct URL fallback");
    }
}
