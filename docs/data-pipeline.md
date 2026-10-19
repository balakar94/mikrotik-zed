# Data pipeline

The command table is never hand-written. It is distilled from MikroTik's
machine-readable CLI reference and embedded into the binary at compile time:

```text
manual.mikrotik.com ──sync_llms.py──▶ llms-full.txt ──extract_commands.py──▶ data/commands.toml ──include_str!()──▶ rsc-ls
                                       (untracked)                          (tracked, generated)
```

## Sync, then extract

```bash
make sync       # fetch llms.txt + llms-full.txt (untracked locals)
make extract    # regenerate data/commands.toml (tracked, generated)
```

`scripts/sync_llms.py --check` (via `make sync-check`) writes nothing and
exits non-zero when upstream moved — that is how CI notices drift.
`make validate` regenerates the table and then runs
`scripts/check_extract_fresh.sh`, which diffs the tracked file while ignoring
the non-deterministic `# Generated:` timestamp header (timestamp-agnostic;
the check also tolerates shallow clones).

## Provenance (never hand-edit)

- `data/commands.toml` header: RouterOS era hint, UTC timestamp, source SHA256,
  index SHA256. Coverage counts live here — link, don't paste.
- `llms.txt` supplies unique page titles and official source URLs, and
  extraction now requires it: without the index every source link would
  disappear, so a missing `llms.txt` aborts the run unless
  `--allow-missing-index` opts into a degraded, recovery-only pass. Source
  separators in the combined corpus keep property sections on their indexed
  page; fenced examples cannot create menus or change ownership. When source
  identity is unavailable, extraction keeps its conservative context fallback.
- Menus carry source URLs and coverage markers. CLI properties inherit their
  menu's source; narrative enrichments, curated overrides, and generic item
  properties retain separate evidence. Every argument states whether `unset`
  support is known, so unknown support stays unknown instead of reading as
  non-unsettable.
- `data/upstream-docs.toml`: sync manifest (SHA256 of both upstream files,
  version, timestamp), regenerated alongside `make sync`.

Unknown-property diagnostics are only downgraded by the coverage marker: a
marker outside the published vocabulary counts as incomplete, never as
complete. Known affected menus carry partial coverage; hierarchy-only menus
carry unknown coverage. Syntax diagnostics retain their normal severity.
Reference coverage means a CLI reference exists, not that upstream
documentation is guaranteed complete.

## Drift workflow

A weekly `docs-drift` workflow (`.github/workflows/docs-drift.yml`)
re-checks upstream against the snapshot and notifies via the
`upstream-docs` labeled issue, auto-closed once re-synced. CI gates
staleness via a timestamp-agnostic `data/commands.toml` diff, failing hard
only when extraction inputs changed and the upstream fetch failed.

## Trust but verify

Cross-check extracted commands against the
[upstream CLI reference](https://manual.mikrotik.com/docs/cli-reference/)
or `/export` on a real router. Truth source:
<https://manual.mikrotik.com/llms-full.txt>.
Details: `.agents/skills/commands-extraction.md`.
