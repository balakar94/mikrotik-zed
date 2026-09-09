# Grammar

RouterOS has no standard tree-sitter grammar, so this project maintains one
in its **own repository** (`tree-sitter-rsc`). This repo keeps an
**untracked working copy** at `grammars/rsc/` and pins the exact revision
in `extension.toml`. Never hand-edit the pin.

## Why a separate repo

- Grammar iterates independently of the extension release cycle.
- The Zed marketplace rejects packages containing nested git repos —
  vendoring history would break publishing.

First clone: `make grammar-clone` (reads the pin from `extension.toml`).
Pinned rev canonical location: `extension.toml` → `[grammars.rsc] rev`.

## Query mirror rule

Only **one** file is mirrored: `languages/rsc/highlights.scm` (canonical,
what Zed loads) ↔ `grammars/rsc/queries/highlights.scm` (so `tree-sitter
test` runs). Equality enforced by
`tests/test_enclosure.py::test_highlights_deduped`. `brackets` / `indents` /
`outline.scm` are Zed-side only; `injections.scm` lives grammar-side only
(intentionally empty, wired via `tree-sitter.json`).

## Corpus workflow

Grammar is the **only** place Node.js enters this project (`tree-sitter-cli`
via npx). Two independent sources of truth — grammar semantics come from
`grammars/rsc/grammar.js`, command data from the extraction pipeline;
never couple them (see [data pipeline](data-pipeline.md)).

```bash
cd grammars/rsc
npx tree-sitter generate       # grammar.js → src/parser.c
npx tree-sitter test           # corpus suite
npx tree-sitter parse FILE     # inspect a parse
npx tree-sitter highlight FILE # preview highlighting
```

Repo shorthands: `make generate`, `make test-grammar`, `make parse FILE=…`,
`make highlight FILE=…`. Generated outputs (`src/parser.c`,
`grammars/rsc/src/*`) are never hand-edited.

## Publish a grammar change

Scripted — validates generation, pushes the grammar repo, updates the pin:

```bash
python scripts/publish_grammar.py --dry-run   # then --push
```

Details: `.agents/skills/tree-sitter-grammar.md`.
