//! `RSC_LS_ALLOW_PATH` policy: may the shim execute an unversioned `rsc-ls`
//! found in `PATH`?
//!
//! Since 0.7.0 the default is **deny**: a fresh install resolves through the
//! checksum-verified work-dir cache or the auto-download path. Only the exact
//! value `1` opts back in for developer builds and manual installs.
//!
//! The decision is split out here because `src/lib.rs` is compiled for
//! `wasm32-wasip2` and is forbidden from using `std::env` or `#[cfg]`
//! (AGENTS.md hard rule #4; `tests/test_enclosure.py` enforces both), and a
//! `Worktree` cannot be constructed in unit tests. `lib.rs` reads the value
//! from `worktree.shell_env()` and delegates the interpretation here.

/// Whether PATH lookups may be used for `shell_env`.
///
/// Deny by default: only a `RSC_LS_ALLOW_PATH` entry whose trimmed value is
/// exactly `1` enables PATH execution. A missing entry, `0`, or any other
/// value denies, so the verified cache/download path always wins unless the
/// user explicitly opts in. Matching is case-sensitive to keep the documented
/// `1`/`0` vocabulary unambiguous.
///
/// `worktree.shell_env()` is an ordered list; like the previous inline
/// `iter().find(..)`, the first matching entry wins.
pub(crate) fn path_lookup_allowed(shell_env: &[(String, String)]) -> bool {
    shell_env
        .iter()
        .find(|(key, _)| key == "RSC_LS_ALLOW_PATH")
        .map(|(_, value)| value.trim() == "1")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn missing_gate_denies_by_default() {
        assert!(!path_lookup_allowed(&[]));
        assert!(!path_lookup_allowed(&env(&[("PATH", "/usr/bin")])));
    }

    #[test]
    fn exact_one_enables_path_lookup() {
        assert!(path_lookup_allowed(&env(&[("RSC_LS_ALLOW_PATH", "1")])));
        // Surrounding whitespace is trimmed before comparison.
        assert!(path_lookup_allowed(&env(&[("RSC_LS_ALLOW_PATH", " 1 ")])));
        assert!(path_lookup_allowed(&env(&[
            ("PATH", "/usr/bin"),
            ("RSC_LS_ALLOW_PATH", "1"),
        ])));
    }

    #[test]
    fn zero_and_other_values_deny() {
        for value in ["0", "", "  ", "true", "yes", "on", "2", "10", "01"] {
            assert!(
                !path_lookup_allowed(&env(&[("RSC_LS_ALLOW_PATH", value)])),
                "value {value:?} must not enable PATH lookup"
            );
        }
    }

    #[test]
    fn first_duplicate_entry_wins() {
        // `shell_env()` is an ordered list; the first entry is authoritative,
        // matching the previous inline `iter().find(..)` behavior.
        let pairs = env(&[("RSC_LS_ALLOW_PATH", "0"), ("RSC_LS_ALLOW_PATH", "1")]);
        assert!(!path_lookup_allowed(&pairs));
        let pairs = env(&[("RSC_LS_ALLOW_PATH", "1"), ("RSC_LS_ALLOW_PATH", "0")]);
        assert!(path_lookup_allowed(&pairs));
    }

    #[test]
    fn key_match_is_exact() {
        assert!(!path_lookup_allowed(&env(&[("RSC_LS_ALLOW_PATH_X", "1")])));
        assert!(!path_lookup_allowed(&env(&[("rsc_ls_allow_path", "1")])));
    }
}
