# Skill: Commands Table Extraction

## When to Use

Trigger keywords: **`extract` | `commands.toml` | `llms` | `sync` | `regenerate` | `menus` | `ArgTable` | `CLI coverage` | `stale commands`**

Also use when: `data/commands.toml` is stale, RouterOS version bumped, CI fails on `git diff --exit-code data/commands.toml`, or completion/hover misses a menu.

## Quick Decision Tree

```
Need to update commands.toml?
├─ llms-full.txt changed on disk?              → python scripts/extract_commands.py
├─ Upstream may have changed?                  → python scripts/sync_llms.py && python scripts/extract_commands.py
├─ Just checking staleness (CI/local)?         → python scripts/sync_llms.py --check  (exit 2 = updates available)
├─ Extraction logic changed?                   → python scripts/extract_commands.py && python -m pytest tests/test_extract_commands.py -v
└─ Single missing menu/property?               → rg in llms-full.txt → patch extract_commands.py → re-extract → add test
```

## Truth Sources

| File | Role | Count |
|------|------|-------|
| `llms-full.txt` | Truth source — full RouterOS docs (version in its header) | see header |
| `llms.txt` | Index with page URLs (llmstxt.org) | see file |
| `data/commands.toml` | Generated — embedded by `rsc-ls` via `include_str!()` | complete CLI; count: `rg -c '^\[\[menus\]\]' data/commands.toml` |
| `scripts/extract_commands.py` | Extraction — CLI-path regex, dedup, metadata header | — |
| `scripts/sync_llms.py` | Sync — fetches from `manual.mikrotik.com` | — |

## How to Sync Upstream

```bash
python scripts/sync_llms.py --check          # dry-run, no writes
# Exit codes: 0 = up-to-date, 1 = fetch/write error, 2 = updates available

python scripts/sync_llms.py                  # fetch (timeout 30s, 3 retries, SHA256 diff)
python scripts/sync_llms.py --force          # overwrite even if hash matches
```

Upstream: `https://manual.mikrotik.com/llms.txt` and `/llms-full.txt`.

**CI integration** — `--check` is CI-friendly (exit `2` without touching disk):

```yaml
- run: python scripts/sync_llms.py --check  # 0 pass, 2 stale, 1 network error
```

Full staleness gate (see `ci.yml`, job that runs extract + diff):

```bash
python scripts/extract_commands.py
git diff --exit-code -- data/commands.toml || (echo "::error::stale — run extract and commit" && false)
```

## How to Regenerate

```bash
python scripts/extract_commands.py
```

- Headings `##`/`###`/`####` via `HEADING_RE = r"^#{2,4}\s+(.+)"` + `_extract_heading_path()` (strips `[link](url)`, trailing `.`, requires `/`)
- Filter via **CLI-path regex** `^[a-z0-9][a-z0-9/_-]*$` on inner path (no spaces, lowercase, empty `_DENY_ROOTS`)
- Covers **ALL roots** — `/interface`, `/ip`, `/ipv6`, `/routing`, `/queue`, `/system`, `/tool`, `/user`, `/certificate`, `/caps-man`, `/container`, `/disk`, `/file`, `/ppp`, `/mpls`, `/radius`, `/snmp`, `/log`, `/dude`, …
- Dedup by `path`, sort, write `data/commands.toml` with **metadata header** (version, UTC timestamp, `sha256[:16]`)

## Full Pipeline — Expected Output

Illustrative output — **numbers vary with each upstream docs release**; never assert a memorized count, always compare against the previous run and the `data/commands.toml` header.

```bash
$ python scripts/sync_llms.py
Fetching https://manual.mikrotik.com/llms.txt ...
  llms.txt: unchanged (hash a1b2c3d4e5f60011)
Fetching https://manual.mikrotik.com/llms-full.txt ...
  llms-full.txt: changed (local dead70b6f2db093f -> remote 9f1a2b3c4d5e6f70)
    version: <old> -> <new>
    lines: <old> -> <new> (+<delta>)
  llms-full.txt: wrote <bytes> bytes
Sync complete: files updated. Run `python3 scripts/extract_commands.py` to regenerate commands.toml.

$ python scripts/extract_commands.py
Parsing /path/to/llms-full.txt...
Found <N> total entries, <N> match target menus.
Wrote /path/to/data/commands.toml (<N> menus)

$ rg -c '^\[\[menus\]\]' data/commands.toml
<N>
$ head -7 data/commands.toml
# MikroTik RouterOS CLI Command Table
# Auto-generated from llms-full.txt
# Covers: ALL roots (complete) — /interface, /ip, /ipv6, /routing, /queue, /system, /tool, /user, /certificate, /caps-man, ...
# RouterOS version: <from upstream header>
# Generated: <UTC timestamp>
# Source hash (sha256[:16]): <hash>
```

If `sync` reports `unchanged`, `commands.toml` header timestamp still updates — use `git diff` to confirm material changes.

## Output Format

```toml
# MikroTik RouterOS CLI Command Table
# Auto-generated from llms-full.txt
# Covers: ALL roots (complete) — /interface, /ip, /ipv6, /routing, /queue, /system, /tool, /user, /certificate, /caps-man, ...
# RouterOS version: 7.22+
# Generated: 2026-08-22T16:37:40.696074Z
# Source hash (sha256[:16]): dead70b6f2db093f

[[menus]]
path = "/ip/firewall/filter"
type = "Directory"
[[menus.flags]]
name = "X"
description = "disabled"
[[menus.arguments]]
name = "chain"
type = "enum (input | forward | output)"
required = true
description = "Chain name"
[[menus.read_only]]
name = "bytes"
type = "num"
```

Per menu: `path`, `type` (`Directory`|`Command`|`Settings Directory`), `flags[]`, `arguments[]` (`required`/`unset`), `read_only[]`. Built by `generate_toml()`.

## Extraction Logic

`parse_llms_full()`:

1. **Headings** — `_extract_heading_path()` on `##`/`###`/`####`; strip links/dots; require `/`; `None` otherwise.
2. **Type** — `**Type:** Directory|Command|Settings Directory`.
3. **ArgTable** — `<ArgTable c1="Flag|Argument|Read-only Argument">` → `current_section`.
4. **Rows** — `<ArgTableRow arg="name" typ="type" mandatory="1" unset="1">desc</ArgTableRow>`.
5. **Finalize** — `should_include()` CLI regex + dedup by path + sort + `generate_toml()` with `_extract_routeros_version()` + `_source_hash()`.

`should_include()`: `startswith("/")` + no spaces + `^[a-z0-9][a-z0-9/_-]*$` + not in `_DENY_ROOTS` (empty for complete coverage).

## Verification

```bash
rg -c '^\[\[menus\]\]' data/commands.toml          # expected = previous count (compare before/after)
rg 'path = "/ip/firewall/filter"' data/commands.toml
rg 'path = "/interface/bridge"' data/commands.toml
rg 'path = "/certificate"' data/commands.toml
rg 'type = ""' data/commands.toml | wc -l          # minimal empty types
python scripts/extract_commands.py && git diff --exit-code data/commands.toml  # staleness
```

## Testing

`tests/test_extract_commands.py` — no network, `tempfile` + `parse_llms_full`:

```bash
python -m pytest tests/test_extract_commands.py -v
python -m pytest tests/test_extract_commands.py::TestShouldInclude -v
python -m pytest tests/test_extract_commands.py::TestParseLlmsFull -v
python -m pytest tests/ -v                          # full suite
```

Classes: `TestShouldInclude` (CLI regex), `TestEscapeTomlString`, `TestCleanType`, `TestGenerateToml`, `TestExtractHeadingPath`, `TestParseLlmsFull` (flags/args/readonly, mandatory/unset). When fixing a missing menu, add a `TestParseLlmsFull` case with a minimal `llms-full.txt` snippet.

## Common Pitfalls

| Pitfall | Symptom | Fix |
|---------|---------|-----|
| Hand-editing `commands.toml` | Lost on next extract | Edit `extract_commands.py` or `llms-full.txt` instead |
| Uppercase/dot path (e.g. `/Backup/Restore`) | Silently dropped | Correct — only `^[a-z0-9][a-z0-9/_-]*$` is valid CLI |
| Link heading `## [ip/foo](url)` | `_extract_heading_path` → `None` | Expected; CLI headings never use link-only syntax |
| ArgTable as markdown table | Menu has 0 args | Rare, not extracted — patch parser or add manual entry |
| `type` truncated `...` | `clean_type()` caps 100 chars | Intentional |
| `sync --check` exit 2 in CI | "updates available" | Run `sync && extract` and commit |
| Duplicate/unstable diff | Dedup/sort missing | Keep the dedup+sort in `generate_toml()` |
| Stale `extension.toml` rev | Wrong grammar in Zed | `python scripts/publish_grammar.py` (grammar skill) |

## Agent Checklist — Before Committing

- [ ] `python scripts/sync_llms.py --check` → `0` (or `sync --force` if `2`)
- [ ] `python scripts/extract_commands.py` → expected menu count
- [ ] `rg -c '^\[\[menus\]\]' data/commands.toml` matches expected
- [ ] Spot-check: `rg 'path = "/<root>' data/commands.toml`
- [ ] `python -m pytest tests/test_extract_commands.py -v` green
- [ ] `python scripts/extract_commands.py && git diff --exit-code data/commands.toml` clean
- [ ] `cargo test -p rsc-ls` if LS embeds `commands.toml`
- [ ] No manual `commands.toml` edits without `# manual:` comment
- [ ] Commit `llms-full.txt` + `data/commands.toml` together if synced

## Rules

- Always regenerate from `llms-full.txt` — never bulk hand-edit `commands.toml`.
- Script is single source of truth for extraction.
- Manual properties need `# manual:` comment with rationale.
- Keep `commands.toml` + `llms-full.txt` committed (versioned snapshots).
- Keep `HEADING_RE` / `_extract_heading_path` canonical — no duplicated regex.
- Keep `^[a-z0-9][a-z0-9/_-]*$` + empty `_DENY_ROOTS` for complete coverage.
