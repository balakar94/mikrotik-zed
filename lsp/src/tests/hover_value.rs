// Hover — property values.
//
// Covers the value-side card: boolean literals, enum members (bounded),
// typed values with the shared example hint, `$var` references, quoted
// strings, and the unattributable cases that must stay silent.
use super::hover_fixtures::*;

/// Synthetic menu exercising every value-card branch, including an `enum`
/// with more members than the card shows.
fn value_data() -> MenuData {
    MenuData::from_toml_str(
        r#"
[[menus]]
path = "/demo/value"
type = "Directory"
[[menus.arguments]]
name = "mode"
type = "enum (alpha | beta | gamma)"
[[menus.arguments]]
name = "many"
type = "enum"
enum_values = ["m00","m01","m02","m03","m04","m05","m06","m07","m08","m09","m10","m11","m12","m13","m14","m15","m16","m17","m18","m19"]
[[menus.arguments]]
name = "ratio"
type = "num"
[[menus.arguments]]
name = "wait"
type = "time"
[[menus.arguments]]
name = "addr"
type = "ipPrefix"
[[menus.arguments]]
name = "note"
type = "string"
[[menus.arguments]]
name = "enabled"
type = "bool"
"#,
    )
}

/// Hover the middle of the first `needle` occurrence on `line`.
fn hover_value(data: &MenuData, line: &str, needle: &str) -> Option<Hover> {
    let pos = line.find(needle).expect("needle present") + needle.len() / 2;
    hover_at(data, line, pos)
}

// ── Enum values ──────────────────────────────────────────────────────────

#[test]
fn test_value_hover_enum_lists_accepted_members() {
    let data = value_data();
    let h = hover_value(&data, "/demo/value add mode=alpha", "alpha").expect("enum value card");
    let v = &h.contents.value;
    assert!(v.contains("**alpha**"), "{v}");
    assert!(v.contains("Value of `mode`"), "{v}");
    assert!(v.contains("Type: `enum (alpha | beta | gamma)`"), "{v}");
    assert!(v.contains("Values: alpha | beta | gamma"), "{v}");
    assert!(v.contains("Source: published reference"), "{v}");
}

#[test]
fn test_value_hover_enum_list_is_bounded() {
    let data = value_data();
    let h = hover_value(&data, "/demo/value add many=m00", "m00").expect("enum card");
    let v = &h.contents.value;
    assert!(v.contains("m00 | m01"), "{v}");
    assert!(v.contains("m11"), "12th member shown: {v}");
    assert!(
        !v.contains("m12"),
        "13th member must fold into the footer: {v}"
    );
    assert!(v.contains("(+8 more)"), "{v}");
}

#[test]
fn test_value_hover_real_alt_wrapper_enum_value() {
    // `new-mss` is an `alt` type whose embedded enum_values carry
    // `clamp-to-pmtu`; the value card must still list it.
    let data = MenuData::load();
    let h = hover_value(
        &data,
        "/ipv6/firewall/mangle/add new-mss=clamp-to-pmtu",
        "clamp-to-pmtu",
    )
    .expect("clamp-to-pmtu card");
    let v = &h.contents.value;
    assert!(v.contains("**clamp-to-pmtu**"), "{v}");
    assert!(v.contains("Value of `new-mss`"), "{v}");
    assert!(v.contains("Values: clamp-to-pmtu"), "{v}");
}

// ── Boolean literals ─────────────────────────────────────────────────────

#[test]
fn test_value_hover_boolean_literals() {
    let data = value_data();
    for (literal, sentence) in [
        ("yes", "enables the option"),
        ("true", "enables the option"),
        ("on", "enables the option"),
        ("no", "disables the option"),
        ("false", "disables the option"),
        ("off", "disables the option"),
    ] {
        let line = format!("/demo/value add enabled={literal}");
        let h = hover_value(&data, &line, literal).expect("bool card");
        let v = &h.contents.value;
        assert!(v.contains(&format!("**{literal}**")), "label: {v}");
        assert!(v.contains(sentence), "{literal} -> {v}");
        assert!(v.contains("Source: published reference"), "{v}");
    }
}

#[test]
fn test_value_hover_bool_nonliteral_uses_typed_card() {
    let data = value_data();
    let h = hover_value(&data, "/demo/value add enabled=maybe", "maybe").expect("typed fallback");
    let v = &h.contents.value;
    assert!(v.contains("**maybe**"), "{v}");
    assert!(v.contains("Value of `enabled=`"), "{v}");
    assert!(v.contains("Type: `bool`"), "{v}");
    assert!(v.contains("Example: `yes`"), "{v}");
}

#[test]
fn test_value_hover_real_slash_and_space_joined_boolean() {
    let data = MenuData::load();
    for line in [
        "/ipv6/address/add advertise=no",
        "/ipv6 address add advertise=no",
    ] {
        let h = hover_value(&data, line, "no").expect("bool value card");
        let v = &h.contents.value;
        assert!(v.contains("**no**"), "{line}: {v}");
        assert!(v.contains("disables the option"), "{line}: {v}");
    }
}

// ── Other typed values ───────────────────────────────────────────────────

#[test]
fn test_value_hover_ip_prefix_shows_example() {
    let data = value_data();
    let h =
        hover_value(&data, "/demo/value add addr=192.168.1.1/24", "192.168.1.1").expect("ip card");
    let v = &h.contents.value;
    assert!(v.contains("**192.168.1.1/24**"), "{v}");
    assert!(v.contains("Value of `addr=`"), "{v}");
    assert!(v.contains("Type: `ipPrefix`"), "{v}");
    assert!(v.contains("Example: `192.168.1.1/24`"), "{v}");
}

#[test]
fn test_value_hover_number_and_duration_types() {
    let data = value_data();
    let num = hover_value(&data, "/demo/value add ratio=1.5", "1.5").expect("num card");
    assert!(num.contents.value.contains("Type: `num`"));
    let time = hover_value(&data, "/demo/value add wait=1h30m", "1h30m").expect("time card");
    assert!(
        time.contents.value.contains("Type: `time`"),
        "{}",
        time.contents.value
    );
    assert!(time.contents.value.contains("Value of `wait=`"));
}

#[test]
fn test_value_hover_quoted_string_strips_quotes() {
    let data = value_data();
    let line = "/demo/value add note=\"hello world\"";
    let h = hover_value(&data, line, "hello").expect("string card");
    let v = &h.contents.value;
    assert!(
        v.contains("**hello world**"),
        "quotes stripped in label: {v}"
    );
    assert!(v.contains("Value of `note=`"), "{v}");
    assert!(v.contains("Type: `string`"), "{v}");
}

#[test]
fn test_value_hover_variable_reference_is_minimal() {
    let data = value_data();
    let h = hover_value(&data, "/demo/value add note=$myVar", "$myVar").expect("var card");
    let v = &h.contents.value;
    assert!(v.contains("**$myVar**"), "{v}");
    assert!(v.contains("Variable reference"), "{v}");
    assert!(v.contains("Source: published reference"), "{v}");
    assert!(!v.contains("Type:"), "variable card stays minimal: {v}");
}

#[test]
fn test_value_hover_real_variable_reference() {
    let data = MenuData::load();
    let h = hover_value(&data, "/ipv6/route/set routing-table=$V6Table", "$V6Table")
        .expect("variable card");
    let v = &h.contents.value;
    assert!(v.contains("**$V6Table**"), "{v}");
    assert!(v.contains("Variable reference"), "{v}");
}

// ── Structural delimiters glued to the value ─────────────────────────────

#[test]
fn test_value_hover_variable_with_trailing_delimiters() {
    // An enclosing expression can glue closers to the value token
    // (`address=$var]] = 0)`); they are not part of the variable name.
    let data = value_data();
    for suffix in ["]]", ")", ",", ";"] {
        let line = format!("/demo/value add note=$myVar{suffix}");
        let h = hover_value(&data, &line, "$myVar").expect("var card");
        let v = &h.contents.value;
        assert!(v.contains("**$myVar**"), "{line}: {v}");
        assert!(
            !v.contains(&format!("$myVar{suffix}")),
            "{line}: delimiter leaked into the label: {v}"
        );
    }
}

#[test]
fn test_value_hover_variable_inside_find_region() {
    let data = value_data();
    let line = "/demo/value remove [find where note=$myVar]]";
    let h = hover_value(&data, line, "$myVar").expect("var card inside find");
    assert!(
        h.contents.value.contains("**$myVar**"),
        "{}",
        h.contents.value
    );
}

#[test]
fn test_value_hover_quoted_value_followed_by_delimiter() {
    let data = value_data();
    let line = "/demo/value add note=\"hi\"]";
    let h = hover_value(&data, line, "hi").expect("quoted value card");
    let v = &h.contents.value;
    assert!(v.contains("**hi**"), "{v}");
    assert!(!v.contains("hi]"), "{v}");
}

#[test]
fn test_value_hover_quoted_value_keeps_internal_bracket() {
    let data = value_data();
    let line = "/demo/value add note=\"a]b\"";
    let pos = line.find("a]b").unwrap();
    let h = hover_at(&data, line, pos).expect("quoted value card");
    assert!(
        h.contents.value.contains("**a]b**"),
        "brackets inside quotes are content: {}",
        h.contents.value
    );
}

#[test]
fn test_value_hover_boolean_with_trailing_delimiter() {
    let data = value_data();
    let line = "/demo/value add enabled=no]";
    let h = hover_value(&data, line, "no").expect("bool card");
    let v = &h.contents.value;
    assert!(v.contains("**no**"), "{v}");
    assert!(v.contains("disables the option"), "{v}");
}

// ── Precedence and None cases ────────────────────────────────────────────

#[test]
fn test_value_hover_property_key_still_uses_property_card() {
    let data = value_data();
    let h = hover_value(&data, "/demo/value add note=hello", "note").expect("key card");
    let v = &h.contents.value;
    assert!(v.contains("**note**"), "{v}");
    assert!(v.contains("Type: `string`"), "{v}");
    assert!(
        !v.contains("Value of `note=`"),
        "key hover must not render a value card: {v}"
    );
}

#[test]
fn test_value_hover_slash_leading_value_is_not_a_menu_card() {
    let data = value_data();
    let h = hover_value(&data, "/demo/value add note=/ip", "/ip").expect("value card wins");
    let v = &h.contents.value;
    assert!(v.contains("**/ip**"), "{v}");
    assert!(v.contains("Value of `note=`"), "{v}");
    assert!(
        !v.contains("### /ip "),
        "slash-leading value must not render as a menu: {v}"
    );
}

#[test]
fn test_value_hover_unattributable_returns_none() {
    let data = value_data();
    assert!(
        hover_value(&data, "/demo/value add bogus=1", "1").is_none(),
        "unknown key"
    );
    assert!(
        hover_value(&data, "/bogus/menu add x=1", "1").is_none(),
        "unknown menu"
    );
    assert!(
        hover_value(&data, "/demo/value remove 0", "0").is_none(),
        "positional selector"
    );
    assert!(
        hover_value(&data, "# alpha beta", "alpha").is_none(),
        "comment text"
    );
    assert!(
        hover_value(&data, ":local x=1", "1").is_none(),
        "no menu context"
    );
}

#[test]
fn test_value_hover_unknown_key_on_known_menu_is_none() {
    let data = MenuData::load();
    assert!(
        hover_value(
            &data,
            "/ipv6/address/add new-mss=clamp-to-pmtu",
            "clamp-to-pmtu"
        )
        .is_none(),
        "a key that is not an argument of the current menu stays silent"
    );
}

// ── Continuation lines ───────────────────────────────────────────────────

#[test]
fn test_value_hover_continuation_lines() {
    let data = MenuData::load();
    let doc = "/ipv6/address/add advertise=no \\\n  comment=test\n";
    let mut lines = doc.lines();
    let line0 = lines.next().unwrap();
    let line1 = lines.next().unwrap();

    let pos0 = line0.find("no").unwrap() + 1;
    let h0 = compute_hover(&data, line0, pos0, doc, 0).expect("value on continued command");
    assert!(
        h0.contents.value.contains("disables the option"),
        "{}",
        h0.contents.value
    );

    let pos1 = line1.find("test").unwrap() + 1;
    let h1 = compute_hover(&data, line1, pos1, doc, 1).expect("value on continuation line");
    let v1 = &h1.contents.value;
    assert!(v1.contains("**test**"), "{v1}");
    assert!(v1.contains("Value of `comment=`"), "{v1}");
}
