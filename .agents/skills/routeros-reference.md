# Skill: RouterOS Command Reference Lookup

## When to Use

Activate this skill whenever the agent must:

- **Lookup** a RouterOS command, menu path, or menu type (`Directory` / `Command` / `Settings Directory`)
- **Verify** a property name, argument, flag, or read-only field before generating `.rsc`
- Handle any prompt mentioning **RouterOS**, **MikroTik**, **RSC**, **CLI reference**, **property**, **type**, **ArgTable**, or **`llms-full`**
- Answer questions about valid values for a type (`enum`, `bool`, `ipAddr`, `time`, …)
- Generate, edit, or review diagnostics/completion/hover behavior in `rsc-ls`

If the user asks for a command you do not recall, **do not answer from memory** — run the lookups below.

## Truth Source Priority

Always resolve in this order. Do not skip a higher priority.

| Priority | Source | Location | Why |
|----------|--------|----------|-----|
| 1 | `data/commands.toml` | repo root | Fast, structured, embedded in `rsc-ls` via `include_str!()`. Menus with `path`/`type`/`arguments`/`read_only`/`flags` + metadata header (`RouterOS version`, `Generated` UTC, `Source hash`). |
| 2 | `llms-full.txt` | repo root | Full RouterOS docs (untracked; fetch via `make sync`; version in its header). Canonical descriptions, ArgTable rows, `mandatory` flags. Use when `commands.toml` looks incomplete. |
| 3 | `llms.txt` | repo root | Doc index with page URLs/summaries. Use to find the public URL for a menu. |

All three are generated from `https://manual.mikrotik.com` — the upstream truth. Validate via `scripts/sync_llms.py --check`.

## Coverage

**Complete CLI** — current coverage: `rg -c '^\[\[menus\]\]' data/commands.toml`. All roots are included via CLI-path regex `^[a-z0-9][a-z0-9/_-]*$`. Implicit parents (e.g., `/ip/firewall` implied by `/ip/firewall/filter`) are allowed by diagnostics.

## How to Lookup — 3 Workflows

### Workflow A: Does this menu / property exist? (fastest)

```bash
# Exact menu
rg -n 'path = "/ip/firewall/filter"' data/commands.toml
rg -n 'path = "/interface/bridge/port"' data/commands.toml

# All children of a menu
rg -n 'path = "/ip/firewall' data/commands.toml

# Does property X exist on menu Y?
rg -A 60 'path = "/ip/firewall/filter"' data/commands.toml | rg -n 'name = "chain"|name = "action"'
rg -A 80 'path = "/ip/address"' data/commands.toml | rg -n 'name = '

# Flags / read-only fields
rg -A 80 'path = "/ip/firewall/filter"' data/commands.toml | rg -n '^\[\[menus\.(flags|read_only)\]\]'
```

### Workflow B: Full argument detail (types, required, description)

```bash
# Complete block for one menu (copy 80-200 lines)
rg -A 120 'path = "/ip/firewall/filter"' data/commands.toml

# Check required / unset / description inline
rg -A 5 'name = "chain"' data/commands.toml | head -20

# Cross-check against llms-full.txt ArgTable rows (same truth, richer prose)
# NOTE: headings use TWO hashes (`## ip/firewall/filter`) — never hardcode a
# remembered line number; it shifts on every upstream sync. Take it fresh:
rg -n "^##+ ip/firewall/filter$" llms-full.txt         # -> heading line number
sed -n '<START>,<END>p' llms-full.txt                  # read section around that line
rg -n 'arg="chain"|arg="action"' llms-full.txt | head
```

`commands.toml` row: `name`, `type`, `description`, `required` (= `mandatory="1"`), `unset` (= `{ }` suffix).
`llms-full.txt` row: `<ArgTableRow arg="chain" typ="enum" mandatory="1">Description</ArgTableRow>`.

### Workflow C: Resolve description / find public docs URL

```bash
# URL for human docs
rg -n "firewall/filter|/ip/firewall" llms.txt
# -> - [Filter](https://manual.mikrotik.com/docs/cli-reference/ip/firewall/filter.md): ...

# Full prose from llms-full.txt (after finding heading line)
rg -n "## ip/firewall/filter|## interface/bridge" llms-full.txt
rg -n "ArgTable" llms-full.txt | head -20

# If stale (CI also runs this)
python scripts/sync_llms.py --check   # exit 2 = updates available
python scripts/sync_llms.py           # fetch latest llms.txt + llms-full.txt
python scripts/extract_commands.py    # regenerate data/commands.toml
```

## Type Reference

| RouterOS `typ` | Example value | Meaning | Zed `rsc-ls` completion (`kind`) |
|---|---|---|---|
| `string` | `comment="uplink"` | Free-form text (quoted) | `PROPERTY` snippet `prop="$1"` |
| `num` | `distance=10` | Integer | `PROPERTY` snippet `prop=$1` |
| `bool` / `boolean` | `yes` / `no` | Boolean (also `true`/`false`) | `ENUM_MEMBER` → `yes`, `no`, `true`, `false` |
| `time` | `timeout=1d12h` | Duration (`1h30m`, `30s`) | `PROPERTY` (no value list) |
| `ipAddr` | `192.168.1.1` | IPv4 address | `ENUM_MEMBER` hint `0.0.0.0/0` |
| `ipPrefix` | `10.0.0.0/24` | CIDR | `ENUM_MEMBER` hint `0.0.0.0/0` |
| `ip6Addr` / `ip6Prefix` | `2001:db8::1` | IPv6 | `PROPERTY` |
| `macAddr` | `AA:BB:CC:DD:EE:FF` | MAC | `PROPERTY` |
| `iface_enum` | `ether1`, `bridge` | Interface name | `ENUM_MEMBER` → `ether1`, `bridge` |
| `enum (a \| b \| c)` | `action=accept` | Enumerated choice | `ENUM_MEMBER` per value (parsed from `enum (...)`) |
| `multi { ... }` | `a,b` | Multi-select / `super` | `PROPERTY` |
| `switch` | `all`, `static` | Boolean filter switch | `PROPERTY` |
| `days` | `7d` | Days count | `PROPERTY` |
| `composite { , }` | read-only | Composite read-only | not completed (read-only) |
| suffix ` { }` / `unset="1"` | — | Can be unset / nullable | flagged `unset: true` |

Diagnostics and hover use the same types: `unknown-property` checks `arguments`/`flags`/`read_only`; `invalid-enum-value` (Hint) checks `enum (...)`.

## Example: Add Firewall Rule — Both Data Sources

Goal: `/ip/firewall/filter add chain=forward action=drop`

**1) Verify in `commands.toml`:**

```bash
rg -A 120 'path = "/ip/firewall/filter"' data/commands.toml
```

Expected (abridged):

```toml
path = "/ip/firewall/filter"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum"
required = true
description = "Specifies to which chain the rule will be added..."
[[menus.arguments]]
name = "action"
type = "enum (accept | jump | return | log | passthrough | add-src-to-address-list | ...)"
```

`chain` is `required = true` → diagnostics rule `missing-required` (Info) fires on `add`/`set` if omitted.

**2) Cross-check in `llms-full.txt`:**

```bash
rg -n "^##+ ip/firewall/filter$" llms-full.txt   # heading line number (changes every sync — take fresh)
sed -n '<START>,<START+150>p' llms-full.txt      # read from that line
```

Expected:

```
### ip/firewall/filter
**Type:** Directory
<ArgTable c1="Argument" ...>
<ArgTableRow arg="chain" typ="enum" mandatory="1">Specifies to which chain...</ArgTableRow>
<ArgTableRow arg="action" typ="enum (accept | jump | ...)">Action to take...</ArgTableRow>
```

**3) URL:** `rg -n "firewall" llms.txt` → `https://manual.mikrotik.com/docs/cli-reference/ip/firewall/filter.md`

**4) Valid `.rsc`:**

```rsc
/ip firewall filter add chain=forward action=drop comment="block forward" disabled=no
```

## Agent Rules: Never Invent

- [ ] **Never invent** a menu path, property name, type, or enum value. If `rg` returns nothing, say "not found in RouterOS 7.22" — do not guess.
- [ ] Always run **Workflow A** before emitting any RouterOS command in an answer or `.rsc` file.
- [ ] Use **exact `rg` commands** above — `rg -n 'path = "/...\"' data/commands.toml` — not keyword fuzzy search alone.
- [ ] Treat an empty `type = ""` as **unknown/complex** — surface as `type: property` and advise checking `llms-full.txt`, do not fabricate `enum` values.
- [ ] For truncated enums (`enum (accept | jump | ...`) the value list is incomplete — warn and point to `llms-full.txt` for the full list.
- [ ] Check `required`, `unset`, and `mandatory` before claiming a property is optional or nullable.
- [ ] Implicit parents are valid: `/ip/firewall` exists if any `/ip/firewall/*` child exists. Do not flag as `unknown-menu`.
- [ ] `commands.toml` and `grammar.js` are independent: grammar validates *syntax*, `commands.toml` validates *semantics*. Do not conflate.
- [ ] If `commands.toml` may be stale, run `python scripts/sync_llms.py --check` and re-extract — never silently use old data.

## Maintenance

```bash
python scripts/sync_llms.py --check   # CI: exit 2 if upstream changed
python scripts/sync_llms.py && python scripts/extract_commands.py
rg -c '^\[\[menus\]\]' data/commands.toml   # must match expected count (compare before/after extract)
```
