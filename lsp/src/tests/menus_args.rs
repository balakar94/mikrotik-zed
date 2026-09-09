//! Menus — arguments.
use crate::menus::*;

#[test]
fn test_root_level_cli_commands_embedded() {
    // Root CLI commands were entirely absent before the bare-root fix;
    // if any goes missing again the LSP silently loses real CLI surface.
    let data = MenuData::load();
    for path in [
        "/import",
        "/password",
        "/quit",
        "/redo",
        "/undo",
        "/beep",
        "/blink",
    ] {
        let menu = data
            .menu_by_path
            .get(path)
            .unwrap_or_else(|| panic!("root command {path} missing from embedded table"));
        assert_eq!(menu.menu_type, "Command", "{path} must stay typed Command");
    }
    // /environment ships alongside the same fix but its upstream page
    // types it as a Directory — pinned verbatim.
    let environment = data
        .menu_by_path
        .get("/environment")
        .expect("/environment embedded");
    assert_eq!(environment.menu_type, "Directory");

    let safe_mode = data
        .menu_by_path
        .get("/safe-mode")
        .expect("/safe-mode embedded");
    assert_eq!(safe_mode.menu_type, "Settings Directory");
}

#[test]
fn test_radius_root_owns_service_secrets_not_monitor() {
    // Child pages ordered BEFORE their parent root upstream used to leak
    // ArgTable rows into the previous entry. service/secret belong to the
    // /radius root; /radius/monitor is stats-only and must stay clean.
    let data = MenuData::load();
    let radius = data
        .menu_by_path
        .get("/radius")
        .expect("/radius menu embedded");
    for name in ["service", "secret"] {
        assert!(
            radius.arguments.iter().any(|a| a.name == name),
            "/radius argument `{name}` missing"
        );
    }
    if let Some(monitor) = data.menu_by_path.get("/radius/monitor") {
        for name in ["service", "secret"] {
            assert!(
                !monitor.arguments.iter().any(|a| a.name == name),
                "/radius/monitor must not inherit /radius `{name}`"
            );
        }
    }
}

#[test]
fn test_children_index_built() {
    let data = MenuData::load();
    let roots = data.child_names_by_parent.get("").expect("root children");
    assert!(!roots.is_empty(), "should have root menus");
    assert!(roots.iter().any(|c| c.path == "/ip"), "missing /ip root");
}

// ── Ancestor prefix set ───────────────────────────────────────

#[test]
fn test_ancestor_prefixes_built_synthetic() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus]]
path = "/ip/firewall/filter"
type = "Directory"
"#,
    );
    // Root sentinel, root segments, and implicit intermediates.
    assert!(data.ancestor_prefixes.contains("/"));
    assert!(data.ancestor_prefixes.contains("/ip"));
    assert!(data.ancestor_prefixes.contains("/ip/firewall"));
    // Full menu paths themselves are NOT ancestors (they are covered by
    // menu_by_path lookups instead).
    assert!(!data.ancestor_prefixes.contains("/ip/address"));
    // Unknown prefixes stay unknown.
    assert!(!data.ancestor_prefixes.contains("/foo"));
    assert!(!data.ancestor_prefixes.contains("/foo/bar"));
}

#[test]
fn test_ancestor_prefixes_real_data() {
    let data = MenuData::load();
    assert!(data.ancestor_prefixes.contains("/"), "root sentinel");
    assert!(
        data.ancestor_prefixes.contains("/ip"),
        "root segment of real menus"
    );
    assert!(
        data.ancestor_prefixes.contains("/ip/firewall"),
        "implicit intermediate"
    );
    // Empty dataset keeps only the root sentinel (fail-safe path parity).
    let empty = MenuData::from_toml_str("");
    assert!(empty.ancestor_prefixes.contains("/"));
    assert_eq!(empty.ancestor_prefixes.len(), 1);
}

// ── enum_values field & member resolution ─────────────────────

#[test]
fn test_enum_values_deserialized_and_defaulted() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/m"
type = "Directory"
[[menus.arguments]]
name = "with-values"
type = "enum (truncated | display)"
enum_values = ["full", "complete", "list"]
[[menus.arguments]]
name = "without-values"
type = "enum (a | b)"
[[menus.arguments]]
name = "plain"
type = "ipPrefix"
"#,
    );
    let menu = data.menu_by_path.get("/m").unwrap();

    let with = menu
        .arguments
        .iter()
        .find(|a| a.name == "with-values")
        .unwrap();
    assert_eq!(with.enum_values, vec!["full", "complete", "list"]);
    // Embedded array wins over whatever the display string parses to.
    assert_eq!(with.enum_members(), vec!["full", "complete", "list"]);

    let without = menu
        .arguments
        .iter()
        .find(|a| a.name == "without-values")
        .unwrap();
    assert!(without.enum_values.is_empty());
    // Fallback: parse members out of the type string.
    assert_eq!(without.enum_members(), vec!["a", "b"]);

    let plain = menu.arguments.iter().find(|a| a.name == "plain").unwrap();
    assert!(plain.enum_values.is_empty());
    assert!(plain.enum_members().is_empty());
}

#[test]
fn test_enum_members_fallback_empty_on_truncated_type() {
    // Mirrors real generated data BEFORE enum_values existed: truncated
    // display string has no closing paren, so the fallback yields nothing
    // rather than garbage.
    let arg = ArgEntry {
        name: "band".to_string(),
        arg_type: "enum (2ghz-b | 2ghz-onlyg | 2ghz-b/g |...".to_string(),
        enum_values: Vec::new(),
        description: String::new(),
        required: false,
        unset: false,
    };
    assert!(arg.enum_members().is_empty());

    // With the embedded array present, members are complete despite the
    // truncated display string.
    let mut fixed = arg.clone();
    fixed.enum_values = vec!["2ghz-b".to_string(), "5ghz-a".to_string()];
    assert_eq!(fixed.enum_members(), vec!["2ghz-b", "5ghz-a"]);
}

#[test]
fn test_real_data_action_has_complete_enum_values() {
    // Root fix verification: the regenerated command table carries a
    // complete member list for /ip/firewall/filter action, whose display
    // string is truncated by the generator's 100-char cap.
    let data = MenuData::load();
    let filter = data.menu_by_path.get("/ip/firewall/filter").expect("menu");
    let action = filter
        .arguments
        .iter()
        .find(|a| a.name == "action")
        .expect("action argument");
    assert!(
        !action.enum_values.is_empty(),
        "action must embed enum_values after regeneration"
    );
    assert!(action.enum_values.iter().any(|v| v == "accept"));
    assert_eq!(action.enum_members(), action.enum_values);
}

// ── ubit member parsing ─────────────────────────────────────

#[test]
fn test_parse_ubit_values_comma_separated() {
    assert_eq!(
        parse_ubit_values("ubit (pap, chap, mschap1, mschap2)"),
        vec!["pap", "chap", "mschap1", "mschap2"]
    );
    assert_eq!(
        parse_ubit_values("ubit (0, 1, 2, 3)"),
        vec!["0", "1", "2", "3"]
    );
    // No space after `ubit` still parses.
    assert_eq!(parse_ubit_values("ubit(a, b)"), vec!["a", "b"]);
}

#[test]
fn test_parse_ubit_values_silent_on_truncated_or_bare() {
    // Truncated display string (generator cap): never guess.
    assert!(parse_ubit_values("ubit (mcs-0, mcs-1, mcs-2, mcs-3, ...").is_empty());
    assert!(parse_ubit_values("ubit (...)").is_empty());
    // Bare `ubit` and unclosed paren carry no member list.
    assert!(parse_ubit_values("ubit").is_empty());
    assert!(parse_ubit_values("ubit (a, b").is_empty());
    assert!(parse_ubit_values("string").is_empty());
    assert!(parse_ubit_values("").is_empty());
}

#[test]
fn test_ubit_members_method_delegates_to_parser() {
    let arg = ArgEntry {
        name: "auth".to_string(),
        arg_type: "ubit (pap, chap)".to_string(),
        enum_values: Vec::new(),
        description: String::new(),
        required: false,
        unset: true,
    };
    assert_eq!(arg.ubit_members(), vec!["pap", "chap"]);
}
