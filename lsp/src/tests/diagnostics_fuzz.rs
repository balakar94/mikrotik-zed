// ── Diagnostics hostile-input fuzz ───────────────────────────────────────
//
// Deterministic property tests over the diagnostics typed-value surface.
//
// The scalar validators (`bool`, `num`, `time`, `macAddr`, IP family,
// `ubit`), the closed-vocabulary `enum` rule, the `missing-required` gate,
// and the `non-unsettable-property` guard all parse untrusted document text.
// These tests pin their fail-closed contract: hostile input never panics,
// malformed values surface the documented code (Hint for open vocabularies,
// the pre-existing Warning for `enum` / `required`), and every diagnostic
// keeps its code and `rsc-ls` source tag.
//
// Determinism: a fixed-seed xorshift64 PRNG (no external crates, no time
// or thread-local randomness) drives the hostile-value generation, and
// short templates get an exhaustive multibyte insertion at every char
// boundary. The test passing is the assertion — no timing budget, no
// environment is touched. Bounded loops keep the whole file well under a
// second in a debug build.

use crate::diagnostics::severity;
use crate::diagnostics::{Diagnostic, compute_diagnostics};
use crate::menus::MenuData;

// ── Deterministic PRNG ───────────────────────────────────────────────────

/// Tiny xorshift64* generator: deterministic, no dependencies.
struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        // A zero state is a fixed point; force a nonzero seed.
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u64() % bound as u64) as usize
        }
    }

    fn pick<'a>(&mut self, items: &'a [&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

/// Multibyte code points inserted at every boundary: two-byte, three-byte,
/// four-byte, and a zero-width combining mark.
const MULTIBYTE: [&str; 4] = ["é", "日", "🌍", "\u{0301}"];

/// Alphabet of hostile fragments for PRNG-generated values. Covers
/// separators, quoting, interpolation, controls, and multibyte input.
const HOSTILE: [&str; 24] = [
    "a", "f", "0", "9", ":", " ", "\t", "\n", "\r", "\0", "/", "\\", "%", "@", "[", "]", "\"", "'",
    "$", "(", ")", ",", "é", "🌍",
];

fn random_string(rng: &mut XorShift64, alphabet: &[&str], max_len: usize) -> String {
    let len = rng.below(max_len + 1);
    let mut s = String::with_capacity(len * 2);
    for _ in 0..len {
        s.push_str(rng.pick(alphabet));
    }
    s
}

/// Insert every multibyte code point at every char boundary of `template`
/// and call `f` for each mutation. Using `char_indices` keeps non-ASCII
/// templates from slicing mid-character.
fn for_each_boundary_insertion(template: &str, mut f: impl FnMut(&str)) {
    let boundaries: Vec<usize> = template
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(template.len()))
        .collect();
    for mb in MULTIBYTE {
        for &i in &boundaries {
            let mutated = format!("{}{}{}", &template[..i], mb, &template[i..]);
            f(&mutated);
        }
    }
}

// ── Shared fixture and severity contract ─────────────────────────────────

fn fuzz_data() -> MenuData {
    MenuData::from_toml_str(
        r#"
[[menus]]
path = "/demo/fuzz"
type = "Directory"
[[menus.arguments]]
name = "flag"
type = "bool"
[[menus.arguments]]
name = "count"
type = "num"
[[menus.arguments]]
name = "period"
type = "time"
[[menus.arguments]]
name = "mac"
type = "macAddr"
[[menus.arguments]]
name = "addr"
type = "ipAddr"
[[menus.arguments]]
name = "rates"
type = "ubit (1Mbps, 2Mbps)"
[[menus.arguments]]
name = "action"
type = "enum (accept | drop)"
[[menus.arguments]]
name = "maybe-unset"
type = "string"
unset = true
[[menus.arguments]]
name = "sticky"
type = "string"
[[menus]]
path = "/demo/req"
type = "Directory"
[[menus.arguments]]
name = "name"
type = "string"
required = true
"#,
    )
}

/// Severity contract shared with the validator pins: pre-existing families
/// keep their Warning/Error tiers; every typed-validator code stays Hint
/// (or Information for the truncation footer), and every diagnostic carries
/// a code plus the `rsc-ls` source tag.
fn assert_fuzz_contract(diags: &[Diagnostic]) {
    const WARNING_CODES: &[&str] = &[
        "unknown-menu",
        "unknown-property",
        "missing-required",
        "duplicate-property",
        "unknown-command",
        "invalid-enum-value",
    ];
    const ERROR_CODES: &[&str] = &["unclosed-brace", "unmatched-brace", "unclosed-quote"];
    for d in diags {
        match d.code.as_deref() {
            Some(c) if WARNING_CODES.contains(&c) => {
                assert_eq!(d.severity, Some(severity::WARNING), "code {c}")
            }
            Some(c) if ERROR_CODES.contains(&c) => {
                assert_eq!(d.severity, Some(severity::ERROR), "code {c}")
            }
            Some("truncated") => {
                assert_eq!(d.severity, Some(severity::INFORMATION), "truncated hint")
            }
            Some(other) => assert!(
                d.severity == Some(severity::HINT) || d.severity == Some(severity::INFORMATION),
                "validator diagnostic '{other}' must be Hint or Information, got {:?}",
                d.severity,
            ),
            None => panic!("every rsc-ls diagnostic carries a code, got {d:?}"),
        }
        assert_eq!(d.source.as_deref(), Some("rsc-ls"), "source tag contract");
    }
}

fn check_doc(data: &MenuData, doc: &str) -> Vec<Diagnostic> {
    let diags = compute_diagnostics(data, doc, "file:///diagnostics-fuzz.rsc");
    assert_fuzz_contract(&diags);
    diags
}

fn has_code(diags: &[Diagnostic], code: &str) -> bool {
    diags.iter().any(|d| d.code.as_deref() == Some(code))
}

// ── bool ─────────────────────────────────────────────────────────────────

#[test]
fn fuzz_bool_never_panics_and_fails_closed() {
    let data = fuzz_data();
    // Fail-closed pin: a malformed bool hints exactly once.
    let diags = check_doc(&data, "/demo/fuzz set flag=maybe");
    assert_eq!(
        diags
            .iter()
            .filter(|d| d.code.as_deref() == Some("invalid-bool-value"))
            .count(),
        1,
        "malformed bool must hint once, got {diags:?}"
    );
    for_each_boundary_insertion("yes", |mutated| {
        check_doc(&data, &format!("/demo/fuzz set flag={mutated}"));
    });
    let mut rng = XorShift64::new(0xb001_0001_dead_beef);
    for _ in 0..2_000 {
        let value = random_string(&mut rng, &HOSTILE, 48);
        check_doc(&data, &format!("/demo/fuzz set flag={value}"));
    }
}

// ── num ──────────────────────────────────────────────────────────────────

#[test]
fn fuzz_num_never_panics_and_fails_closed() {
    let data = fuzz_data();
    let diags = check_doc(&data, "/demo/fuzz set count=notanum");
    assert!(
        has_code(&diags, "invalid-num-value"),
        "malformed num must hint, got {diags:?}"
    );
    for_each_boundary_insertion("1500", |mutated| {
        check_doc(&data, &format!("/demo/fuzz set count={mutated}"));
    });
    let mut rng = XorShift64::new(0xb001_0002_dead_beef);
    for _ in 0..2_000 {
        let value = random_string(&mut rng, &HOSTILE, 48);
        check_doc(&data, &format!("/demo/fuzz set count={value}"));
    }
}

// ── time ─────────────────────────────────────────────────────────────────

#[test]
fn fuzz_time_never_panics_and_fails_closed() {
    let data = fuzz_data();
    let diags = check_doc(&data, "/demo/fuzz set period=notatime");
    assert!(
        has_code(&diags, "invalid-time-value"),
        "malformed time must hint, got {diags:?}"
    );
    for_each_boundary_insertion("1h30m", |mutated| {
        check_doc(&data, &format!("/demo/fuzz set period={mutated}"));
    });
    let mut rng = XorShift64::new(0xb001_0003_dead_beef);
    for _ in 0..2_000 {
        let value = random_string(&mut rng, &HOSTILE, 48);
        check_doc(&data, &format!("/demo/fuzz set period={value}"));
    }
}

// ── macAddr ──────────────────────────────────────────────────────────────

#[test]
fn fuzz_mac_never_panics_and_fails_closed() {
    let data = fuzz_data();
    let diags = check_doc(&data, "/demo/fuzz set mac=zz:zz:zz:zz:zz:zz");
    assert!(
        has_code(&diags, "invalid-mac-value"),
        "malformed MAC must hint, got {diags:?}"
    );
    for_each_boundary_insertion("AA:BB:CC:DD:EE:FF", |mutated| {
        check_doc(&data, &format!("/demo/fuzz set mac={mutated}"));
    });
    let mut rng = XorShift64::new(0xb001_0004_dead_beef);
    for _ in 0..2_000 {
        let value = random_string(&mut rng, &HOSTILE, 48);
        check_doc(&data, &format!("/demo/fuzz set mac={value}"));
    }
}

// ── IP family ────────────────────────────────────────────────────────────

#[test]
fn fuzz_ip_never_panics_and_fails_closed() {
    let data = fuzz_data();
    let diags = check_doc(&data, "/demo/fuzz set addr=999.999.999.999");
    assert!(
        has_code(&diags, "invalid-ip-value"),
        "malformed IP must hint, got {diags:?}"
    );
    for_each_boundary_insertion("10.0.0.1", |mutated| {
        check_doc(&data, &format!("/demo/fuzz set addr={mutated}"));
    });
    let mut rng = XorShift64::new(0xb001_0005_dead_beef);
    for _ in 0..2_000 {
        let value = random_string(&mut rng, &HOSTILE, 48);
        check_doc(&data, &format!("/demo/fuzz set addr={value}"));
    }
}

// ── ubit ─────────────────────────────────────────────────────────────────

#[test]
fn fuzz_ubit_never_panics_and_fails_closed() {
    let data = fuzz_data();
    let diags = check_doc(&data, "/demo/fuzz set rates=notabits!!");
    assert!(
        has_code(&diags, "invalid-ubit-value"),
        "malformed ubit must hint, got {diags:?}"
    );
    for_each_boundary_insertion("1Mbps", |mutated| {
        check_doc(&data, &format!("/demo/fuzz set rates={mutated}"));
    });
    let mut rng = XorShift64::new(0xb001_0006_dead_beef);
    for _ in 0..2_000 {
        let value = random_string(&mut rng, &HOSTILE, 48);
        check_doc(&data, &format!("/demo/fuzz set rates={value}"));
    }
}

// ── enums ────────────────────────────────────────────────────────────────

#[test]
fn fuzz_enums_never_panic_and_fail_closed() {
    let data = fuzz_data();
    // Fail-closed pin: a value outside the closed vocabulary warns.
    let diags = check_doc(&data, "/demo/fuzz set action=bogus");
    let hit = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("invalid-enum-value"))
        .expect("unknown enum value must warn, got {diags:?}");
    assert_eq!(hit.severity, Some(severity::WARNING));
    for_each_boundary_insertion("accept", |mutated| {
        check_doc(&data, &format!("/demo/fuzz set action={mutated}"));
    });
    let mut rng = XorShift64::new(0xb001_0007_dead_beef);
    for _ in 0..2_000 {
        let value = random_string(&mut rng, &HOSTILE, 48);
        // Hostile enum input stays silent or warns with the enum code —
        // never a new tier, never an untagged diagnostic.
        check_doc(&data, &format!("/demo/fuzz set action={value}"));
    }
}

// ── required ─────────────────────────────────────────────────────────────

#[test]
fn fuzz_required_never_panics_and_fails_closed() {
    let data = fuzz_data();
    // Fail-closed pin: omitting a required property warns; supplying it
    // stays silent on the required code.
    let missing = check_doc(&data, "/demo/req add");
    assert!(
        has_code(&missing, "missing-required"),
        "omitted required property must warn, got {missing:?}"
    );
    let present = check_doc(&data, "/demo/req add name=ether1");
    assert!(
        !has_code(&present, "missing-required"),
        "supplied required property must stay silent, got {present:?}"
    );
    for_each_boundary_insertion("ether1", |mutated| {
        check_doc(&data, &format!("/demo/req add name={mutated}"));
    });
    let mut rng = XorShift64::new(0xb001_0008_dead_beef);
    for _ in 0..2_000 {
        // Hostile keys and values: unknown keys warn with the pre-existing
        // unknown-property code, never a new tier.
        let key = random_string(&mut rng, &HOSTILE, 24);
        let value = random_string(&mut rng, &HOSTILE, 48);
        check_doc(&data, &format!("/demo/req add {key}={value}"));
    }
}

// ── unset ────────────────────────────────────────────────────────────────

#[test]
fn fuzz_unset_never_panics_and_fails_closed() {
    let data = fuzz_data();
    // Fail-closed pin: unsetting a non-unsettable property hints with the
    // unsettable mark; an unsettable one stays silent.
    let bad = check_doc(&data, "/demo/fuzz unset 0 sticky");
    let hit = bad
        .iter()
        .find(|d| d.code.as_deref() == Some("non-unsettable-property"))
        .expect("non-unsettable target must hint, got {bad:?}");
    assert_eq!(hit.severity, Some(severity::HINT));
    assert!(
        hit.message.contains("unsettable: no"),
        "got {:?}",
        hit.message
    );
    let ok = check_doc(&data, "/demo/fuzz unset 0 maybe-unset");
    assert!(
        !has_code(&ok, "non-unsettable-property"),
        "unsettable target must stay silent, got {ok:?}"
    );
    for_each_boundary_insertion("maybe-unset", |mutated| {
        check_doc(&data, &format!("/demo/fuzz unset 0 {mutated}"));
    });
    let mut rng = XorShift64::new(0xb001_0009_dead_beef);
    for _ in 0..2_000 {
        let target = random_string(&mut rng, &HOSTILE, 48);
        check_doc(&data, &format!("/demo/fuzz unset 0 {target}"));
    }
}
