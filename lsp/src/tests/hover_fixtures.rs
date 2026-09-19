// Hover — shared fixtures (both TOML variants + hover_at helpers).
//
// Fixtures only: every test lives in its aspect file (hover_menu,
// hover_property, hover_verbs, hover_edge, hover_builtin), so this module
// carries no `fn test_*` without `#[test]`.
pub(crate) use crate::hover::*;
pub(crate) use crate::menus::MenuData;
pub(crate) fn synthetic_data() -> MenuData {
    let toml_str = r#"
[[menus]]
path = "/ip/address"
type = "Directory"

[[menus.arguments]]
name = "address"
type = "ipPrefix"
description = "IP address"

[[menus.arguments]]
name = "interface"
type = "iface_enum"

[[menus.arguments]]
name = "comment"
type = "string"

[[menus.arguments]]
name = "no-type-prop"
type = ""

[[menus.flags]]
name = "X"
description = "disabled"

[[menus.flags]]
name = "D"
description = ""

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
path = "/interface/bridge"
type = "Directory"

[[menus]]
path = "/empty/menu"
type = "Directory"
"#;
    MenuData::from_toml_str(toml_str)
}

// ── Helpers for hover tests ──────────────────────────────────────────────

pub(crate) fn hover_at(data: &MenuData, line: &str, character: usize) -> Option<Hover> {
    // Single-line doc helper
    compute_hover(data, line, character, line, 0)
}

/// Compact fixture for word-boundary, verb, and property tests.
pub(crate) fn synth() -> MenuData {
    MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = "ipPrefix"
description = "IP address"
[[menus.arguments]]
name = "interface"
type = "iface_enum"
[[menus.flags]]
name = "X"
description = "disabled"
[[menus.flags]]
name = "D"
description = ""
[[menus]]
path = "/ip/firewall/filter"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum (input | forward | output)"
[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"
"#,
    )
}
