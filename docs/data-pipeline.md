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

A sync replaces both documents and the manifest as one recoverable batch, and
validates before replacing anything: empty bodies, HTML error pages, invalid
UTF-8, oversized downloads, missing documentation structure, retried
HTTP-success error pages, and substantial section-count loss are refused, so a
bad fetch cannot leave mixed provenance. An ordinary rename failure rolls back
the completed writes; each rename is atomic per file, not across a process
crash affecting the entire batch.

Unknown-property diagnostics are only downgraded by the coverage marker: a
marker outside the published vocabulary counts as incomplete, never as
complete. Known affected menus carry partial coverage; hierarchy-only menus
carry unknown coverage. Syntax diagnostics retain their normal severity.
Reference coverage means a CLI reference exists, not that upstream
documentation is guaranteed complete.

## Coverage reports and regression checks

`make extract` writes a deterministic JSON report to
`.cache/mikrotik-zed/extraction-report.json` (untracked). For every menu it
records the property names, the coverage marker, the source URL and how many
properties carry no evidence at all; for every parsed property table it
records the source URL, heading, current line number, candidate and resolved
menus, property names, unresolved reason and aggregate statistics. CI uploads
this report for inspection, including when later verification fails.

```bash
make extract EXTRACT_ARGS='--report /tmp/extraction-before.json'
# After changing extraction logic or syncing upstream:
make extract EXTRACT_ARGS='--baseline /tmp/extraction-before.json --fail-on-regression'
# Verify the generated command table without writing files:
make extract EXTRACT_ARGS='--check'
```

The baseline gate compares per menu, never per aggregate: a dataset-wide URL
total can hold while single menus lose their page. It reports removed menus,
removed properties, a menu that lost or changed its source URL, a coverage
downgrade, properties that lost their source, and newly unresolved tables.
Table identities use source, heading and property names instead of volatile
line numbers, so shifted lines are not regressions. Baselines are versioned:
a report written by an older schema is rejected rather than reinterpreted.
A reviewed upstream removal may require accepting a new baseline, and reports
can contain unresolved tables even when extraction succeeds — ambiguous data
is reported instead of assigned to a guessed menu.

An unrelated menu documenting the same property name does not prove that a
skipped table is covered. Coverage is checked against its candidate menus.

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
