# Skill: Docs Maintenance

## When to Use

Trigger this skill when the task involves any of: editing `docs/*.md`, `README.md` coverage/install sections, `scripts/check_docs.py`, `make docs` / `make docs-check`, pasting versions/counts/revs into prose, or reviewing a PR that touches user-facing documentation.

For the docs build design see `docs/index.md`; for gates see `qa-ci-release.md`; for RouterOS command truth see `routeros-reference.md`.

## The Split (never mix layers)

| Layer | Audience | Rule |
| ----- | -------- | ---- |
| `README.md` | landing + links | Teaser only: pitch, features, install, quickstart, pointers. Never the manual. |
| `docs/*.md` | user manual | Behavior, env reference, pipelines, troubleshooting. Facts by reference (below). |
| `.agents/skills/` | agents | Imperative deep dives. Never duplicate `docs/` prose; link to the page instead. |
| `CHANGELOG.md` / `ROADMAP.md` | history / direction | What shipped / what is staged. Docs pages link here, never restate. |

## Volatile Facts — Look Up, Never Quote

These rot on contact. Reference the canonical file; never paste the value:

| Fact | Canonical location |
| ---- | ------------------ |
| Grammar `rev` | `extension.toml` → `[grammars.rsc] rev` |
| Extension version | `extension.toml` |
| Menu coverage / RouterOS snapshot | `data/commands.toml` header |
| Upstream doc version | `llms-full.txt` header |
| Toolchain / WASM target | `rust-toolchain.toml` |
| Make targets | `make help` |
| Test counts | run the suite |
| Caps / timeouts | `lsp/src/caps.rs` (sole source of truth) |

`scripts/check_docs.py` enforces this with a literal ban (versions, 40-hex SHAs, `N menus`, `RouterOS 7.x` outside fences). Caps values (`MiB`, `MAX_*`, `RSC_LS_*`, `MIKROTIK_*`) and `0!live_` are allowlisted by construction. History (`CHANGELOG.md`, `docs/adr/`) and fenced code blocks are exempt; anything else needs `<!-- volatile-ok: reason -->`.

## The Gate

```bash
make docs-check   # hygiene + relative links/anchors + index reachability + volatile ban (stdlib only, offline, <30s)
make docs         # no build — plain Markdown; preview hint only
```

Checks: trailing whitespace / EOF newline / heading jumps (first heading is `#`); every `[text](target)` resolves on disk with anchors slugged GitHub-style (no absolute `/docs/...` links); every `docs/*.md` reachable from `docs/index.md` (`adr/`, `export-fixtures/` and `publishing-runbook.md` only need an explicit index link). Wired into `make validate` after `check-manifest` and the CI `Docs • check` job.

## Adding or Changing a Page

1. Create `docs/<page>.md` with a `#` title; link it from `docs/index.md` (Start here or Canonical references) or the gate fails reachability.
2. Link relatively (`quickstart.md#2-binary-resolution`); verify the anchor slug matches the target heading.
3. Cite sources, don't fork them: caps from `caps.rs`, env from `lsp/src/live.rs`, tasks from `languages/rsc/tasks.json`, CLI from `--help`.
4. Run `make docs-check` before claiming done.

## Related Skills

* `qa-ci-release.md` — gates, suites, release validation.
* `language-convention.md` — English-only boundary.
* `zed-extension-dev` — manifest/packaging (docs/ ships as text only, never packaged).
