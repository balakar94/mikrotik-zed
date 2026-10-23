// ── Startup banner ───────────────────────────────────────────────────────
//
// The banner is the first thing a stale cached binary gets wrong: it prints
// the version of the *binary* and the provenance of the *embedded dataset*,
// so these pins keep both fields readable and unambiguous.

use crate::dataset_banner_line;
use crate::menus::{DatasetProvenance, dataset_provenance};

fn prov(era_hint: &str, src_hash: &str) -> DatasetProvenance {
    DatasetProvenance {
        era_hint: era_hint.to_string(),
        src_hash: src_hash.to_string(),
    }
}

#[test]
fn banner_line_names_count_hint_and_digest_in_one_sentence() {
    assert_eq!(
        dataset_banner_line(1125, &prov("7.26", "2704a9b0c03ad827")),
        "dataset: 1125 menus, era hint 7.26 (highest upstream RouterOS mention), \
         src 2704a9b0c03ad827"
    );
}

#[test]
fn banner_line_states_the_era_hint_scope_exactly_once() {
    // "dataset" and the era-hint explanation each appear once: the previous
    // wording (`dataset: upstream docs era hint …`) said the same thing three
    // times and still left the semantics implicit.
    let line = dataset_banner_line(1, &prov("7.26", "abc"));
    assert_eq!(line.matches("dataset").count(), 1, "{line}");
    assert_eq!(line.matches("era hint").count(), 1, "{line}");
    assert!(!line.contains("upstream docs"), "{line}");
}

#[test]
fn banner_line_keeps_degraded_provenance_visible() {
    // A header without the era-hint/source lines degrades to `unknown`; the
    // banner must still print a well-formed line rather than drop a field.
    assert_eq!(
        dataset_banner_line(0, &prov("unknown", "unknown")),
        "dataset: 0 menus, era hint unknown (highest upstream RouterOS mention), \
         src unknown"
    );
}

#[test]
fn banner_line_keeps_the_embedded_provenance_shape() {
    // Shape pin over the real table: the field order is what makes a stale
    // binary unmistakable when compared with `data/commands.toml`.
    let line = dataset_banner_line(7, &dataset_provenance());
    assert!(line.starts_with("dataset: 7 menus, era hint "), "{line}");
    assert!(
        line.contains(" (highest upstream RouterOS mention), src "),
        "{line}"
    );
    assert!(
        line.ends_with(dataset_provenance().src_hash.as_str()),
        "{line}"
    );
}
