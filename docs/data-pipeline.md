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
`make validate` asserts extract idempotency
(`git diff --exit-code data/commands.toml`).

## Provenance (never hand-edit)

- `data/commands.toml` header: RouterOS version, UTC timestamp, source SHA256.
  Coverage counts live here — link, don't paste.
- `data/upstream-docs.toml`: sync manifest (SHA256 of both upstream files,
  version, timestamp), regenerated alongside `make sync`.

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
