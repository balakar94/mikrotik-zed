// White-box: parser.
use crate::parser::*;
fn slash_verb_data() -> MenuData {
    MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ipv6/nd/prefix"
type = "Directory"
[[menus.arguments]]
name = "interface"
type = "iface_enum"
[[menus.arguments]]
name = "prefix"
type = "string"
[[menus.arguments]]
name = "comment"
type = "string"
[[menus]]
path = "/log"
type = "Directory"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = "ipPrefix"
[[menus.arguments]]
name = "interface"
type = "iface_enum"
[[menus.arguments]]
name = "comment"
type = "string"
"#,
    )
}

use crate::menus::MenuData;

fn synthetic_data() -> MenuData {
    MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = "ipPrefix"
[[menus.arguments]]
name = "interface"
type = "iface_enum"
[[menus]]
path = "/ip/route"
type = "Directory"
[[menus.arguments]]
name = "gateway"
type = "ipAddr"
[[menus]]
path = "/ip/firewall/filter"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum (input | forward | output)"
[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"
[[menus]]
path = "/interface/bridge/port"
type = "Directory"
[[menus]]
path = "/system/clock"
type = "Directory"
"#,
    )
}

// ── tokenize ─────────────────────────────────────────────────────────────

#[test]
fn test_split_key_value_rejects_quote_blind_equals() {
    // `=` inside quotes is literal content, not a separator.
    assert!(split_key_value(r#"("a=b""#).is_none());
    assert!(split_key_value("$x").is_none());
    assert!(split_key_value("[find").is_none());
    assert!(split_key_value("=value").is_none());
    assert!(split_key_value("9bad=x").is_none());
    assert!(split_key_value("foo.bar=x").is_none());
    assert!(split_key_value("no-equals").is_none());
}

#[test]
fn test_split_key_value_first_outside_equals_wins() {
    // Quoted value containing `=` keeps the outer key.
    assert_eq!(
        split_key_value(r#"comment="a=b c=d""#),
        Some(("comment", r#""a=b c=d""#))
    );
}

#[test]
fn test_parse_line_slash_verb_shorthand() {
    let data = slash_verb_data();
    let ctx = parse_line(&data, "/ipv6/nd/prefix/add interface=bridge");
    assert_eq!(ctx.path, "/ipv6/nd/prefix");
    assert_eq!(ctx.command.as_deref(), Some("add"));
    assert_eq!(
        ctx.properties.get("interface").map(|s| s.as_str()),
        Some("bridge")
    );
}

#[test]
fn test_parse_line_slash_verb_never_overwrites_explicit_verb() {
    let data = slash_verb_data();
    let ctx = parse_line(&data, "/ipv6/nd/prefix print");
    assert_eq!(ctx.path, "/ipv6/nd/prefix");
    assert_eq!(ctx.command.as_deref(), Some("print"));
    // Bare `/add` (empty parent) never splits.
    let ctx2 = parse_line(&data, "/add");
    assert_eq!(ctx2.command, None);
}

#[test]
fn test_parse_line_log_info_with_quoted_equals_has_no_properties() {
    let data = slash_verb_data();
    let ctx = parse_line(&data, r#"/log info ("digi prevPd=" . $x)"#);
    assert_eq!(ctx.path, "/log");
    assert_eq!(ctx.command.as_deref(), Some("info"));
    assert!(
        ctx.properties.is_empty(),
        "quoted `=` must not spawn properties, got {:?}",
        ctx.properties
    );
}

#[test]
fn test_parse_line_bracket_find_ignored_command_stays_set() {
    let data = slash_verb_data();
    let ctx = parse_line(
        &data,
        "/ip/address set [find pool-name=digi-ipv6] address=1.1.1.1",
    );
    assert_eq!(ctx.path, "/ip/address");
    assert_eq!(ctx.command.as_deref(), Some("set"));
    assert!(
        !ctx.properties.contains_key("pool-name"),
        "inner bracket key must not leak, got {:?}",
        ctx.properties
    );
    assert_eq!(
        ctx.properties.get("address").map(|s| s.as_str()),
        Some("1.1.1.1")
    );
}

#[test]
fn test_parse_line_quoted_comment_still_property() {
    let data = slash_verb_data();
    let ctx = parse_line(&data, r#"/ip/address add comment="a=b c=d""#);
    assert_eq!(
        ctx.properties.get("comment").map(|s| s.as_str()),
        Some(r#""a=b c=d""#)
    );
}

#[test]
fn test_parse_line_concat_comment_still_property() {
    let data = slash_verb_data();
    let ctx = parse_line(&data, r#"/ip/address add comment=("X old=" . $y)"#);
    assert_eq!(ctx.command.as_deref(), Some("add"));
    assert!(
        ctx.properties.contains_key("comment"),
        "concat comment must stay a property, got {:?}",
        ctx.properties
    );
}

#[test]
fn test_parse_cache_bounded_by_max_docs_discipline() {
    let mut cache = ParseCache::new();
    for i in 0..(crate::MAX_DOCS + 25) {
        let uri = format!("file:///cache-cap-{i}.rsc");
        cache.lookup_or_insert(&uri, ":put hi\n");
    }
    assert!(
        cache.entries.len() <= crate::MAX_DOCS,
        "cache must never outgrow the document store, got {}",
        cache.entries.len()
    );
}

#[test]
fn test_parse_cache_edit_invalidates_via_hash_mismatch() {
    let mut cache = ParseCache::new();
    let uri = "file:///cache-edit.rsc";
    let before = ":local x\n:put $x\n";
    let after = ":local x\n:put $x\n:put $x\n";
    cache.lookup_or_insert(uri, before);
    assert!(cache.lookup(uri, before).is_some());
    // Same URI, changed text: the stored hash no longer matches, so the
    // lookup misses (stale entries can never be served)…
    assert!(
        cache.lookup(uri, after).is_none(),
        "edited text must miss the cache"
    );
    // …and the next access reparses the new content.
    let texts: Vec<String> = cache
        .lookup_or_insert(uri, after)
        .iter()
        .map(|ll| ll.text().to_string())
        .collect();
    assert_eq!(texts.len(), 3);
    assert!(cache.lookup(uri, after).is_some());
    assert!(cache.lookup(uri, before).is_none());
}

#[test]
fn test_parse_cache_invalidate_drops_entry_regardless_of_content() {
    let mut cache = ParseCache::new();
    let uri = "file:///cache-close.rsc";
    let doc = ":put hi\n";
    cache.lookup_or_insert(uri, doc);
    assert!(cache.lookup(uri, doc).is_some());
    cache.invalidate(uri);
    assert!(
        cache.lookup(uri, doc).is_none(),
        "didClose must kill the entry even for unchanged text"
    );
    // Invalidating an unknown URI is a no-op, never a panic.
    cache.invalidate("file:///never-opened.rsc");
}

#[test]
fn test_split_key_value_valid_keys() {
    assert_eq!(split_key_value("pool-name=x"), Some(("pool-name", "x")));
    assert_eq!(split_key_value("tcp-flags=syn"), Some(("tcp-flags", "syn")));
    assert_eq!(
        split_key_value("start-date=nov/01/2024"),
        Some(("start-date", "nov/01/2024"))
    );
    assert_eq!(
        split_key_value("place-before=0"),
        Some(("place-before", "0"))
    );
    assert_eq!(split_key_value("chain="), Some(("chain", "")));
}
