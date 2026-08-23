// ── Data structures and indices for the RSC language server ────────
//
// Loads commands.toml at compile time via include_str!() and builds
// all necessary lookup structures (path index, parent→children index,
// implicit root entries).

use serde::Deserialize;
use std::collections::{HashMap, HashSet};

// ── Embedded command table ────────────────────────────────────────

const COMMANDS_TOML: &str = include_str!("../../data/commands.toml");

// ── TOML data structures ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CommandsFile {
    #[serde(default)]
    pub(crate) menus: Vec<RawMenuEntry>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawMenuEntry {
    path: String,
    #[serde(rename = "type", default)]
    menu_type: String,
    #[serde(default)]
    flags: Vec<RawArgEntry>,
    #[serde(default)]
    arguments: Vec<RawArgEntry>,
    #[serde(default)]
    read_only: Vec<RawArgEntry>,
}

#[derive(Debug, Deserialize)]
struct RawArgEntry {
    name: String,
    #[serde(rename = "type", default)]
    arg_type: String,
    /// Complete enum members extracted upstream by the generator from the RAW
    /// (untruncated) type string. Absent for non-enum types and for types the
    /// docs list without members.
    #[serde(default)]
    enum_values: Vec<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    unset: bool,
}

#[derive(Debug, Clone)]
pub struct MenuEntry {
    pub path: String,
    pub menu_type: String,
    pub flags: Vec<ArgEntry>,
    pub arguments: Vec<ArgEntry>,
    pub read_only: Vec<ArgEntry>,
}

#[derive(Debug, Clone)]
pub struct ArgEntry {
    pub name: String,
    pub arg_type: String,
    pub enum_values: Vec<String>,
    pub description: String,
    pub required: bool,
    /// Upstream docs mark whether the property can be removed again with
    /// `unset` (1007 entries in the generated table carry it). No rule
    /// consumes it yet; it is kept so the embedded dataset round-trips the
    /// generated schema without silent field loss.
    #[allow(dead_code)]
    pub unset: bool,
}

impl ArgEntry {
    /// Enum members usable for completion and validation.
    ///
    /// Prefers the complete embedded `enum_values` array; falls back to
    /// parsing the display type string (synthetic/test data, or entries whose
    /// upstream docs carried no member list — those parse to empty when the
    /// display string was truncated by the generator's 100-char cap).
    pub fn enum_members(&self) -> Vec<String> {
        if !self.enum_values.is_empty() {
            return self.enum_values.clone();
        }
        parse_enum_values(&self.arg_type)
    }
}

/// Parse enum members out of a display type string such as
/// `enum (input | forward | output)`.
///
/// Fallback only: the display string may be truncated (trailing `...`) by the
/// generator, in which case this returns whatever fits or nothing at all.
/// Complete members come from [`ArgEntry::enum_values`] instead.
pub(crate) fn parse_enum_values(type_str: &str) -> Vec<String> {
    let inner = type_str
        .strip_prefix("enum")
        .and_then(|s| s.trim().strip_prefix('('))
        .and_then(|s| s.strip_suffix(')'));
    match inner {
        Some(body) => body.split('|').map(|s| s.trim().to_string()).collect(),
        None => Vec::new(),
    }
}

// ── Child entry (for populating implicit children) ────────────────

#[derive(Debug, Clone)]
pub struct ChildEntry {
    pub name: String,
    pub path: String,
    pub menu_type: String,
}

// ── Context (output of parse_line) ────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct LineContext {
    pub path: String,
    pub command: Option<String>,
    /// property name → value (empty string if just "key=")
    pub properties: HashMap<String, String>,
    pub last_token: String,
}

// ── Global state ──────────────────────────────────────────────────

pub struct MenuData {
    pub menus: Vec<MenuEntry>,
    pub menu_by_path: HashMap<String, MenuEntry>,
    pub child_names_by_parent: HashMap<String, Vec<ChildEntry>>,
    /// Every path that is a proper ancestor prefix of at least one known
    /// menu (e.g. "/", "/ip", "/ip/firewall"), precomputed for O(1)
    /// "is this a known menu prefix?" checks in diagnostics. Kept separate
    /// from `child_names_by_parent` so the membership semantics ("ancestor
    /// of a real menu") do not depend on how the children index is keyed.
    pub ancestor_prefixes: HashSet<String>,
}

impl MenuData {
    pub fn load() -> Self {
        match toml::from_str::<CommandsFile>(COMMANDS_TOML) {
            Ok(commands) => Self::from_commands(commands),
            Err(e) => {
                eprintln!("[rsc-ls] FATAL: embedded commands.toml failed to parse: {e}");
                // Fail-safe: return empty dataset rather than panicking and crashing LSP.
                // This prevents a supply-chain corrupted TOML from causing an unrecoverable panic.
                MenuData {
                    menus: Vec::new(),
                    menu_by_path: HashMap::new(),
                    child_names_by_parent: HashMap::new(),
                    ancestor_prefixes: HashSet::new(),
                }
            }
        }
    }

    /// Build `MenuData` from an arbitrary TOML string (useful for deterministic tests).
    ///
    /// Test-only: production loads the embedded table via [`MenuData::load`],
    /// so this constructor is compiled out of release builds (an unused pub
    /// item in a binary crate would trip `dead_code` under `-D warnings`).
    #[cfg(test)]
    pub fn from_toml_str(s: &str) -> Self {
        let commands: CommandsFile =
            toml::from_str(s).expect("failed to parse TOML string in from_toml_str");
        Self::from_commands(commands)
    }

    fn from_commands(commands: CommandsFile) -> Self {
        let menus: Vec<MenuEntry> = commands
            .menus
            .into_iter()
            .filter_map(|raw| {
                // Validate path: must be non-empty, start with '/', contain only safe chars.
                // Reject traversal, control chars, or overly long paths (DoS via crafted TOML).
                let path = raw.path.trim().to_string();
                if path.is_empty() || !path.starts_with('/') {
                    eprintln!("[rsc-ls] skipping menu with invalid path: {path:?}");
                    return None;
                }
                if path.len() > 256 {
                    eprintln!(
                        "[rsc-ls] skipping menu with overly long path ({}): {path:?}",
                        path.len()
                    );
                    return None;
                }
                if path.contains('\0')
                    || path.contains("..")
                    || path.chars().any(|c| c.is_control())
                {
                    eprintln!("[rsc-ls] skipping menu with suspicious path: {path:?}");
                    return None;
                }
                // Basic allowlist for path characters
                if !path
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
                {
                    eprintln!("[rsc-ls] skipping menu with non-allowlisted path chars: {path:?}");
                    return None;
                }
                Some(MenuEntry {
                    path,
                    menu_type: raw.menu_type,
                    flags: raw.flags.into_iter().map(Into::into).collect(),
                    arguments: raw.arguments.into_iter().map(Into::into).collect(),
                    read_only: raw.read_only.into_iter().map(Into::into).collect(),
                })
            })
            .collect();

        let mut menu_by_path: HashMap<String, MenuEntry> = HashMap::new();
        for m in &menus {
            menu_by_path.insert(m.path.clone(), m.clone());
        }

        // Build parent→children index from ALL paths, and collect every
        // proper ancestor prefix for O(1) known-prefix membership checks.
        let mut ancestor_prefixes: HashSet<String> = HashSet::new();
        let mut child_map: HashMap<String, HashMap<String, ChildEntry>> = HashMap::new();

        for m in &menus {
            let parts: Vec<&str> = m.path.split('/').collect();
            for i in 2..parts.len() {
                let parent_path = format!("/{}", parts[1..i].join("/"));
                ancestor_prefixes.insert(parent_path.clone());
                let child_name = parts[i].to_string();
                let child_path = format!("/{}", parts[1..i + 1].join("/"));

                let entry = child_map.entry(parent_path).or_default();

                let child = entry
                    .entry(child_name.clone())
                    .or_insert_with(|| ChildEntry {
                        name: child_name,
                        path: child_path,
                        menu_type: m.menu_type.clone(),
                    });
                if m.menu_type == "Directory" || m.menu_type == "Settings Directory" {
                    child.menu_type = m.menu_type.clone();
                }
            }
            // Root segments are ancestors too (e.g. "/ip" for "/ip/address").
            if let Some(root_name) = m.path.split('/').nth(1) {
                ancestor_prefixes.insert(format!("/{root_name}"));
            }
        }

        // "/" itself is always a known prefix (the old check treated it as
        // known whenever the children index existed at all).
        ancestor_prefixes.insert("/".to_string());
        let mut root_children: HashMap<String, ChildEntry> = HashMap::new();
        for m in &menus {
            if let Some(root_name) = m.path.split('/').nth(1) {
                let root_name = root_name.to_string();
                root_children
                    .entry(root_name.clone())
                    .or_insert_with(|| ChildEntry {
                        name: root_name.clone(),
                        path: format!("/{root_name}"),
                        menu_type: "Directory".to_string(),
                    });
            }
        }
        child_map.insert(String::new(), root_children);

        let child_names_by_parent: HashMap<String, Vec<ChildEntry>> = child_map
            .into_iter()
            .map(|(k, v)| (k, v.into_values().collect()))
            .collect();

        MenuData {
            menus,
            menu_by_path,
            child_names_by_parent,
            ancestor_prefixes,
        }
    }

    /// Standard RouterOS verbs available on most Directory-type menus
    pub const STANDARD_VERBS: &'static [&'static str] = &[
        "add",
        "remove",
        "set",
        "get",
        "print",
        "enable",
        "disable",
        "find",
        "comment",
        "move",
        "export",
        "import",
        "edit",
        "reset",
        "force-update",
    ];
}

// ── Conversions from raw (Deserialize) to clean types ────────────

impl From<RawArgEntry> for ArgEntry {
    fn from(raw: RawArgEntry) -> Self {
        ArgEntry {
            name: raw.name,
            arg_type: raw.arg_type,
            enum_values: raw.enum_values,
            description: raw.description,
            required: raw.required,
            unset: raw.unset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_commands_toml() -> &'static str {
        r#"
[[menus]]
path = "/ip/address"
type = "Directory"

[[menus.arguments]]
name = "address"
type = "ipPrefix"
description = "The IP address and network mask"

[[menus.arguments]]
name = "interface"
type = "iface_enum"

[[menus.flags]]
name = "X"
description = "disabled"

[[menus.flags]]
name = "D"
description = "dynamic"

[[menus.read_only]]
name = "actual-interface"
type = "iface_enum"
description = "The actual interface"

[[menus]]
path = "/ip/route"
type = "Directory"

[[menus.arguments]]
name = "gateway"
type = "address (flags=46ivL)"

[[menus]]
path = "/ip/route/check"
type = "Command"

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
path = "/routing/bgp/connection"
type = "Directory"

[[menus]]
path = "/system/identity"
type = "Directory"
"#
    }

    #[test]
    fn test_parse_commands_toml() {
        let commands: CommandsFile =
            toml::from_str(test_commands_toml()).expect("should parse TOML");
        assert!(!commands.menus.is_empty(), "should have menus");
        assert!(commands.menus.len() >= 4, "should have at least 4 menus");
    }

    #[test]
    fn test_empty_commands_toml() {
        let toml_str = "\n[[menus]]\npath = \"/empty\"\ntype = \"Directory\"\n";
        let commands: CommandsFile = toml::from_str(toml_str).unwrap();
        assert_eq!(commands.menus.len(), 1);
        assert_eq!(commands.menus[0].path, "/empty");
    }

    #[test]
    fn test_menus_are_not_empty() {
        let data = MenuData::load();
        assert!(
            !data.menus.is_empty(),
            "embedded commands.toml should have menus"
        );
        assert!(data.menus.len() >= 50, "should have at least 50 menus");
        assert!(
            !data.menu_by_path.is_empty(),
            "menu_by_path should be populated"
        );
    }

    #[test]
    fn test_all_menus_have_path() {
        let data = MenuData::load();
        for menu in &data.menus {
            assert!(!menu.path.is_empty(), "every menu should have a path");
            assert!(
                menu.path.starts_with('/'),
                "paths should start with /: {}",
                menu.path
            );
        }
    }

    #[test]
    fn test_target_root_menus_present() {
        let data = MenuData::load();
        let paths: Vec<&str> = data.menus.iter().map(|m| m.path.as_str()).collect();

        assert!(
            paths.iter().any(|p| p.starts_with("/ip/")),
            "missing /ip entries"
        );
        assert!(
            paths.iter().any(|p| p.starts_with("/ipv6/")),
            "missing /ipv6 entries"
        );
        assert!(
            paths.iter().any(|p| p.starts_with("/interface/")),
            "missing /interface entries"
        );
        assert!(
            paths.iter().any(|p| p.starts_with("/routing/")),
            "missing /routing entries"
        );
    }

    #[test]
    fn test_no_unwanted_root_menus() {
        // Under complete coverage, /certificate and other previously excluded
        // roots are now included. Verify that.
        let data = MenuData::load();
        assert!(
            data.menus
                .iter()
                .any(|m| m.path.starts_with("/certificate")),
            "should contain /certificate under complete coverage, got {} menus",
            data.menus.len()
        );
    }

    #[test]
    fn test_specific_menus_exist() {
        let data = MenuData::load();

        assert!(
            data.menu_by_path.contains_key("/ip/address"),
            "missing /ip/address"
        );
        assert!(
            data.menu_by_path.contains_key("/ip/route"),
            "missing /ip/route"
        );
        assert!(
            data.menu_by_path.contains_key("/ip/firewall/filter"),
            "missing /ip/firewall/filter"
        );
        assert!(data.menu_by_path.contains_key("/ip/dns"), "missing /ip/dns");
        assert!(
            data.menu_by_path.contains_key("/ip/service"),
            "missing /ip/service"
        );
        assert!(
            data.menu_by_path.contains_key("/ipv6/address"),
            "missing /ipv6/address"
        );
        assert!(
            data.menu_by_path.contains_key("/ipv6/route"),
            "missing /ipv6/route"
        );
        assert!(
            data.menu_by_path.contains_key("/interface/bridge"),
            "missing /interface/bridge"
        );
        assert!(
            data.menu_by_path.contains_key("/interface/ethernet"),
            "missing /interface/ethernet"
        );
        assert!(
            data.menu_by_path.contains_key("/routing/ospf"),
            "missing /routing/ospf"
        );
        assert!(
            data.menu_by_path.contains_key("/routing/bgp"),
            "missing /routing/bgp"
        );

        assert!(
            data.menu_by_path.contains_key("/system/clock"),
            "missing /system/clock"
        );
        assert!(
            data.menu_by_path.contains_key("/tool/ping"),
            "missing /tool/ping"
        );
        assert!(
            data.menu_by_path.contains_key("/queue/simple"),
            "missing /queue/simple"
        );
        assert!(
            data.menu_by_path.contains_key("/user/aaa"),
            "missing /user/aaa"
        );
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
}
