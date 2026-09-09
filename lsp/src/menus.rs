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
    pub(crate) path: String,
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
    /// `unset` (991 entries in the generated table carry it). Consumed by
    /// the diagnostics `non-unsettable-property` Hint, which warns when
    /// `unset` targets a property whose entry carries `unset=false`.
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

    /// Ubit members usable for Hint-only validation.
    ///
    /// Ubit entries carry no embedded member array (the generator only
    /// embeds `enum_values` for `enum` types), so this always parses the
    /// display type string via [`parse_ubit_values`]. Truncated or
    /// member-less types yield an empty list and callers stay silent.
    pub fn ubit_members(&self) -> Vec<String> {
        parse_ubit_values(&self.arg_type)
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

/// Parse ubit members out of a display type string such as
/// `ubit (pap, chap, mschap1, mschap2)`.
///
/// Ubit lists are comma-separated (unlike `enum`, which uses `|`); a
/// trailing `{ ... }` bitmask legend is cut before splitting. Truncated
/// display strings (containing `...` from the generator's display cap) or a
/// missing member list (bare `ubit`, unclosed paren) yield an empty list so
/// callers stay silent rather than guessing.
pub(crate) fn parse_ubit_values(type_str: &str) -> Vec<String> {
    // Cut any `{ ... }` legend the generator may append, then refuse
    // truncated input outright.
    let without_legend = type_str.split('{').next().unwrap_or(type_str);
    if without_legend.contains("...") {
        return Vec::new();
    }
    let inner = without_legend
        .strip_prefix("ubit")
        .and_then(|s| s.trim().strip_prefix('('))
        .and_then(|s| s.strip_suffix(')'));
    match inner {
        Some(body) => body
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
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

/// Provenance of the embedded dataset for the startup banner.
pub struct DatasetProvenance {
    pub version: String,
    pub src_hash: String,
}

/// Provenance parsed from the generated header of the embedded table.
///
/// Independent of the TOML body parse, so it stays available on the
/// fail-safe empty path. Prefixes must match `scripts/extract_commands.py`
/// header output; unknown fields degrade to `"unknown"` (never panic).
pub fn dataset_provenance() -> DatasetProvenance {
    parse_provenance(COMMANDS_TOML)
}

pub(crate) fn parse_provenance(text: &str) -> DatasetProvenance {
    let mut version = "unknown".to_string();
    let mut src_hash = "unknown".to_string();
    for line in text.lines().take(12) {
        if let Some(v) = line.strip_prefix("# RouterOS version:") {
            version = v.trim().to_string();
        } else if let Some(h) = line.strip_prefix("# Source hash (sha256[:16]):") {
            src_hash = h.trim().to_string();
        }
    }
    DatasetProvenance { version, src_hash }
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
