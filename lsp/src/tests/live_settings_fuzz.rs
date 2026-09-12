// Deterministic property tests over the live-settings string surface.
//
// A prior audit found two char-boundary panics on this surface: the
// `server_proto.rs` parse-error id recovery and
// `live_config::parse_fingerprint`. These tests pin the fail-closed
// contract for every parser reachable from environment / workspace
// settings so the F1 (char-boundary) class cannot regress.
//
// Determinism: a fixed-seed xorshift64 PRNG (no external crates, no time
// or thread-local randomness) drives the hostile-string generation, and
// short templates get an EXHAUSTIVE multibyte insertion at every char
// boundary. The test passing is the assertion — no timing budget. The
// only environment mutation is the existing serialized
// `with_settings_transport_env` helper used around the overlay entry
// point. Bounded loops keep the whole file well under a second in a
// debug build.

use crate::caps::{LIVE_CUSTOM_RESOURCES_MAX, LIVE_MAX_HOSTS, LIVE_TIMEOUT_SECS};
use crate::live::*;
use std::collections::HashMap;

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

/// Alphabet of hostile fragments for PRNG-generated strings.
const HOSTILE: [&str; 18] = [
    "a", "f", "0", "9", ":", " ", "\t", "\n", "\r", "\0", "/", "\\", "%", "@", "[", "]", "é", "🌍",
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
/// and call `f` for each mutation. Templates are ASCII in every current
/// caller, so char boundaries equal byte boundaries there; using
/// `char_indices` keeps non-ASCII templates from slicing mid-character.
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

fn cfg_with_env(pairs: &[(&str, &str)]) -> LiveConfig {
    let map: HashMap<&str, &str> = pairs.iter().copied().collect();
    LiveConfig::from_env_with(|k| map.get(k).map(|v| (*v).to_string()))
}

// ── parse_fingerprint ────────────────────────────────────────────────────

#[test]
fn fuzz_parse_fingerprint_never_panics_and_fails_closed() {
    // Unset/blank are a documented no-op, never "invalid".
    assert_eq!(parse_fingerprint(None), (None, false));
    assert_eq!(parse_fingerprint(Some("")), (None, false));
    assert_eq!(parse_fingerprint(Some("   ")), (None, false));

    // Exhaustive multibyte insertion at every boundary of short templates.
    let templates = [
        "sha256:",
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "sha256:aa:bb cc",
    ];
    for tmpl in templates {
        for_each_boundary_insertion(tmpl, |mutated| {
            let (parsed, invalid) = parse_fingerprint(Some(mutated));
            // Contract: a malformed value can never yield a pin.
            assert!(
                parsed.is_none() || !invalid,
                "invalid input must not yield a pin: {mutated:?}"
            );
        });
    }

    // The exact valid shape still parses to 32 bytes.
    let valid = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    assert_eq!(parse_fingerprint(Some(valid)).0.map(|b| b.len()), Some(32));
    assert!(!parse_fingerprint(Some(valid)).1);

    // PRNG hostile strings: never panic, malformed stays fail-closed.
    let mut rng = XorShift64::new(0x5eed_1234_abcd_ef01);
    for _ in 0..2_000 {
        let s = random_string(&mut rng, &HOSTILE, 96);
        let (parsed, invalid) = parse_fingerprint(Some(&s));
        if invalid {
            assert!(parsed.is_none());
        }
    }

    // Huge lengths stay bounded (scan is linear, result fail-closed).
    let huge = "a".repeat(200_000);
    assert_eq!(parse_fingerprint(Some(&huge)), (None, true));
    let huge_mb = format!("sha256:{}é", "a".repeat(200_000));
    assert_eq!(parse_fingerprint(Some(&huge_mb)), (None, true));
}

// ── host / user validation ───────────────────────────────────────────────

#[test]
fn fuzz_host_and_user_validation_never_panics() {
    let mut rng = XorShift64::new(0x0bad_c0de_dead_beef);
    for _ in 0..2_000 {
        let s = random_string(&mut rng, &HOSTILE, 80);
        let _ = validate_host(&s);
        let _ = validate_host_with_allow(&s, false);
        let _ = validate_host_with_allow(&s, true);
        // A rejected user never yields a non-empty result, and any accepted
        // user obeys the documented 1..=64 ASCII allowlist.
        if let Some(u) = validate_user(&s) {
            assert!(!u.is_empty() && u.len() <= 64);
        }
    }

    // Exhaustive multibyte insertion into host/user templates.
    for tmpl in ["router.local", "192.168.88.1", "[::1]", "example.com."] {
        for_each_boundary_insertion(tmpl, |mutated| {
            let _ = validate_host(mutated);
            let _ = validate_host_with_allow(mutated, false);
            let _ = validate_host_with_allow(mutated, true);
            let _ = validate_user(mutated);
        });
    }

    // Fail-closed contract for URI delimiters, controls, null, overlong.
    let long = "a".repeat(254);
    for bad in [
        "a\0b",
        "a\nb",
        "a\rb",
        "a\tb",
        "a b",
        "a@b",
        "a?b",
        "a#b",
        "a%b",
        long.as_str(),
    ] {
        assert!(
            validate_host_with_allow(bad, true).is_err(),
            "host must reject {bad:?}"
        );
        assert!(
            validate_host_with_allow(bad, false).is_err(),
            "host must reject {bad:?}"
        );
    }

    // Trailing dots, brackets, and multibyte hosts never panic; the policy
    // still decides the outcome.
    for host in [
        "router.local.",
        "10.0.0.1.",
        "[::1].",
        "[",
        "]",
        "[]",
        "fe80::1",
        "é",
        "routeré.local",
    ] {
        let _ = validate_host_with_allow(host, false);
        let _ = validate_host_with_allow(host, true);
    }

    // Multi-host env parsing is capped and never panics on hostile input.
    let many: Vec<String> = (0..(LIVE_MAX_HOSTS + 12))
        .map(|i| format!("h{i}"))
        .collect();
    let joined = many.join(",");
    let cfg = cfg_with_env(&[("MIKROTIK_HOST", &joined)]);
    assert!(cfg.hosts.len() <= LIVE_MAX_HOSTS, "host cap");
    let hostile = format!("{}{}", "é,".repeat(50), "router.local");
    let _ = cfg_with_env(&[("MIKROTIK_HOST", &hostile)]);
}

// ── custom resources JSON ────────────────────────────────────────────────

#[test]
fn fuzz_custom_resources_json_never_panics_and_caps() {
    // Malformed, mistyped, traversal, and non-UTF-8-ish inputs all resolve
    // fail-closed (never panic, never exceed the cap).
    let malformed = [
        "",
        "   ",
        "{",
        "[",
        "]",
        "not json",
        "[}]",
        "{]",
        "null",
        "42",
        "true",
        r#"[{"property":1,"path":"/rest","field":"x"}]"#,
        r#"[{"property":"p","path":2,"field":"x"}]"#,
        r#"[{"property":"p","path":"/etc/passwd","field":"f"}]"#,
        r#"[{"property":"p","path":"/rest/../secret","field":"f"}]"#,
        r#"[{"property":"p","path":"/rest","field":"f"},null,1,"x",{}]"#,
        r#"[{"property":"\uD800","path":"/rest","field":"f"}]"#,
        r#"[{"property":"\u0000","path":"/rest","field":"f"}]"#,
    ];
    for raw in malformed {
        let cfg = cfg_with_env(&[("RSC_LS_LIVE_RESOURCES", raw)]);
        assert!(
            cfg.custom_resources.len() <= LIVE_CUSTOM_RESOURCES_MAX,
            "custom-resource cap for {raw:?}"
        );
    }

    // Deep nesting: serde_json's recursion limit rejects it -> empty list.
    let deep = format!("{}0{}", "[".repeat(5_000), "]".repeat(5_000));
    assert!(
        cfg_with_env(&[("RSC_LS_LIVE_RESOURCES", &deep)])
            .custom_resources
            .is_empty()
    );

    // Huge identifier strings are rejected by the per-entry allowlist.
    let huge_property = format!(
        r#"[{{"property":"{}","path":"/rest/interface","field":"name"}}]"#,
        "a".repeat(500_000)
    );
    assert!(
        cfg_with_env(&[("RSC_LS_LIVE_RESOURCES", &huge_property)])
            .custom_resources
            .is_empty()
    );

    // The settings overlay takes an arbitrary `serde_json::Value`; wrong
    // types and hostile strings must not panic and must stay capped.
    let mut rng = XorShift64::new(0xfeed_face_cafe_0001);
    for _ in 0..500 {
        let value = random_json_value(&mut rng, 0);
        let mut cfg = LiveConfig::from_env_with(|_| None);
        LiveConfig::apply_settings_value_with_transport(&mut cfg, &value, true);
        assert!(cfg.custom_resources.len() <= LIVE_CUSTOM_RESOURCES_MAX);
    }
    for value in [
        serde_json::json!({"rsc": {"live": {"custom_resources": "not an array"}}}),
        serde_json::json!({"rsc": {"live": {"custom_resources": {"property": "p"}}}}),
        serde_json::json!({"rsc": {"live": {"custom_resources": [1, 2, null]}}}),
        serde_json::json!({"rsc": {"live": {"RSC_LS_LIVE_RESOURCES": "{}"}}}),
    ] {
        let mut cfg = LiveConfig::from_env_with(|_| None);
        LiveConfig::apply_settings_value_with_transport(&mut cfg, &value, true);
        assert!(cfg.custom_resources.len() <= LIVE_CUSTOM_RESOURCES_MAX);
    }
}

// ── scheme resolution and timeout clamping ───────────────────────────────

#[test]
fn fuzz_scheme_and_timeout_clamping() {
    let mut rng = XorShift64::new(0x1234_5678_9abc_def0);
    for _ in 0..2_000 {
        let port = rng.next_u64() as u16;
        let force = rng.next_u64() & 1 == 0;
        let ssl = rng.next_u64() & 1 == 0;
        // The scheme is always one of the two literals, never a panic.
        assert!(matches!(resolve_scheme(port, force, ssl), "http" | "https"));
        let (scheme, _fired) =
            resolve_scheme_with_legacy(port, force, ssl, rng.next_u64() & 1 == 0);
        assert!(matches!(scheme, "http" | "https"));
    }

    // Timeout: default on unset, clamp to 1..=30 on every hostile form.
    assert_eq!(
        LiveConfig::from_env_with(|_| None).timeout_secs,
        LIVE_TIMEOUT_SECS
    );
    let long = "9".repeat(100_000);
    for raw in [
        "",
        " ",
        "0",
        "-1",
        "1",
        "30",
        "31",
        "999999999999999999999999",
        "5.5",
        "abc",
        "１２３",
        "é",
        long.as_str(),
    ] {
        let cfg = cfg_with_env(&[("MIKROTIK_TIMEOUT", raw)]);
        assert!(
            (1..=30).contains(&cfg.timeout_secs),
            "timeout clamp for {raw:?} -> {}",
            cfg.timeout_secs
        );
    }

    // Port: default 443 on unset, always a nonzero in-range value otherwise.
    assert_eq!(LiveConfig::from_env_with(|_| None).port, 443);
    for raw in [
        "",
        "x",
        "-5",
        "0",
        "1",
        "65535",
        "65536",
        "99999999999999999999",
    ] {
        let cfg = cfg_with_env(&[("MIKROTIK_PORT", raw)]);
        assert!(cfg.port >= 1, "port for {raw:?} -> {}", cfg.port);
    }
}

// ── settings overlay entry point ─────────────────────────────────────────

#[test]
fn fuzz_apply_settings_value_hostile_shapes_never_panic() {
    // Random nested JSON values (objects/arrays/scalars) must never panic,
    // and settings can never smuggle a password or an unclamped timeout.
    // The overlay entry point reads `RSC_LS_ALLOW_SETTINGS_TRANSPORT`, so it
    // runs under the shared env mutex to keep parallel tests deterministic.
    let mut rng = XorShift64::new(0xa5a5_5a5a_c3c3_3c3c);
    let random_values: Vec<serde_json::Value> =
        (0..2_000).map(|_| random_json_value(&mut rng, 0)).collect();
    with_settings_transport_env(false, || {
        for value in &random_values {
            let mut cfg = LiveConfig::from_env_with(|_| None);
            LiveConfig::apply_settings_value(&mut cfg, value);
            assert!(cfg.pass.is_empty(), "settings must never set pass");
            assert!((1..=30).contains(&cfg.timeout_secs));
        }

        // Targeted hostile shapes around every recognized scope/key.
        for value in [
            serde_json::json!(null),
            serde_json::json!([1, 2, 3]),
            serde_json::json!("string"),
            serde_json::json!(42),
            serde_json::json!({"rsc": []}),
            serde_json::json!({"rsc": "live"}),
            serde_json::json!({"mikrotik": []}),
            serde_json::json!(
                {"settings": {"settings": {"settings": {"rsc": {"live": {"host": "h"}}}}}}
            ),
            serde_json::json!({"rsc": {"live": {"host": ["evil", "x"]}}}),
            serde_json::json!({"rsc": {"live": {"user": {"nested": "evil"}}}}),
            serde_json::json!({"rsc": {"live": {"port": -1}}}),
            serde_json::json!({"rsc": {"live": {"port": 70000}}}),
            serde_json::json!({"rsc": {"live": {"timeout": "not a number"}}}),
            serde_json::json!({"rsc": {"live": {"fingerprint": "sha256:é"}}}),
            serde_json::json!({"rsc": {"live": {"ca_file": "🌍"}}}),
            serde_json::json!({"rsc": {"live": {"host": "a\u{0}b"}}}),
        ] {
            let mut cfg = LiveConfig::from_env_with(|_| None);
            LiveConfig::apply_settings_value(&mut cfg, &value);
            assert!(cfg.pass.is_empty());
        }

        // `from_settings_value` runs the full env + overlay entry point; it
        // must survive hostile values (process env is whatever the runner has).
        for value in [
            serde_json::json!({"rsc": {"live": {"host": "router.local", "timeout": 999}}}),
            serde_json::json!({"rsc": {"live": {"custom_resources": [{"property": "p"}]}}}),
            serde_json::json!({"mikrotik": {"port": "not-a-port"}}),
        ] {
            let cfg = LiveConfig::from_settings_value(&value);
            assert!((1..=30).contains(&cfg.timeout_secs));
        }
    });
}

// ── custom path validator ────────────────────────────────────────────────

#[test]
fn fuzz_custom_path_validation_never_panics() {
    // Exhaustive multibyte insertion into the valid path template.
    for_each_boundary_insertion("/rest/ip/firewall/mangle", |mutated| {
        let _ = is_valid_custom_path(mutated);
    });

    // Fail-closed examples that must never be accepted.
    let long = format!("/rest/{}", "a".repeat(200));
    for bad in [
        "",
        "/restx",
        "/rest/../secret",
        "/rest//interface",
        "/etc/passwd",
        "/rest/%2e%2e",
        "/rest/a\\b",
        "/rest/a?b",
        "/rest/a#b",
        "/rest/a@b",
        "/rest/a\u{0}b",
        long.as_str(),
    ] {
        assert!(!is_valid_custom_path(bad), "must reject {bad:?}");
    }
    assert!(is_valid_custom_path("/rest"));
    assert!(is_valid_custom_path("/rest/interface"));
}

// ── Protocol id recovery (sibling F1 surface) ────────────────────────────

#[test]
fn fuzz_extract_id_for_parse_error_never_panics() {
    // The other char-boundary panic named by the audit: raw-body id recovery
    // must never land mid-character on multibyte / invalid-UTF-8 input.
    for tmpl in [
        r#"{"id":"x"}"#,
        r#"{"id":123}"#,
        r#"{"id":null}"#,
        r#"{"id":"#,
        r#"{"id" : "é"}"#,
    ] {
        for_each_boundary_insertion(tmpl, |mutated| {
            let _ = crate::server_proto::extract_id_for_parse_error(mutated.as_bytes());
        });
    }

    // Raw byte soup, including invalid UTF-8.
    let mut rng = XorShift64::new(0x1357_9bdf_2468_ace0);
    for _ in 0..2_000 {
        let len = rng.below(64);
        let bytes: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xff) as u8).collect();
        let _ = crate::server_proto::extract_id_for_parse_error(&bytes);
    }

    // Documented best-effort recovery still works for well-formed scalars,
    // and invalid UTF-8 falls back to null.
    assert_eq!(
        crate::server_proto::extract_id_for_parse_error(br#"{"id":"abc"}x"#),
        serde_json::json!("abc")
    );
    assert_eq!(
        crate::server_proto::extract_id_for_parse_error(br#"{"id":7}x"#),
        serde_json::json!(7)
    );
    assert_eq!(
        crate::server_proto::extract_id_for_parse_error(b"not json"),
        serde_json::Value::Null
    );
    assert_eq!(
        crate::server_proto::extract_id_for_parse_error(&[0xff, 0xfe]),
        serde_json::Value::Null
    );
}

// ── Random JSON value generator ──────────────────────────────────────────
/// Bounded random `serde_json::Value` with hostile keys/strings. Depth is
/// capped so the generator itself cannot overflow the stack.
fn random_json_value(rng: &mut XorShift64, depth: u32) -> serde_json::Value {
    if depth >= 4 {
        return serde_json::Value::String(random_string(rng, &HOSTILE, 32));
    }
    match rng.below(7) {
        0 => serde_json::Value::Null,
        1 => serde_json::Value::Bool(rng.next_u64() & 1 == 0),
        2 => serde_json::Value::from(rng.next_u64()),
        3 => serde_json::Value::from(rng.next_u64() as f64),
        4 => serde_json::Value::String(random_string(rng, &HOSTILE, 48)),
        5 => {
            let n = rng.below(4);
            serde_json::Value::Array((0..n).map(|_| random_json_value(rng, depth + 1)).collect())
        }
        _ => {
            let keys = [
                "host",
                "user",
                "port",
                "ssl_verify",
                "force_http",
                "timeout",
                "custom_resources",
                "pass",
                "ca_file",
                "fingerprint",
                "allow_loopback",
                "rsc",
                "live",
                "mikrotik",
                "settings",
                "MIKROTIK_HOST",
                "MIKROTIK_PASS",
                "RSC_LS_LIVE_ALLOW_LOOPBACK",
            ];
            let mut map = serde_json::Map::new();
            for _ in 0..rng.below(4) {
                map.insert(
                    rng.pick(&keys).to_string(),
                    random_json_value(rng, depth + 1),
                );
            }
            serde_json::Value::Object(map)
        }
    }
}
