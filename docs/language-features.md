# Language features

All contextual features are driven by the command database embedded in
`rsc-ls` (see [data pipeline](data-pipeline.md)). Triggers: `/`, space,
`=` and `:`.

## Completion tiers

`sortText` is `<tier><match>_<label>`; tier is the major key
(source: `lsp/src/completion.rs`, `RankTier`):

| Tier | Prefix | Contents |
| --- | --- | --- |
| Live | `0!live_` | device truth from live cache (`!` sorts before any `0…`); detail `live — …` |
| Required prop | `0` | required arguments first |
| Optional prop | `1` | remaining arguments |
| Verb | `2` | `add`, `set`, … accepted by the menu |
| Sub-menu | `3` | child paths in context |
| Enum value | `4` | documented enum/bool members |
| Common hint | `5` | curated values (e.g. firewall chains), one tier below true members |
| Placeholder | `6` | honest type placeholder (`0.0.0.0/0`) when nothing better exists |
| Flag | `7` | single-letter flags |
| Demoted | `8` | typo fallback when the prefix matched nothing |
| Snippet | `9` | statement snippets, single shared key (curated order) |

Within a tier, match quality orders: exact < prefix < substring.
Responses are capped (`MAX_COMPLETION_ITEMS` — see [caps](lsp-config.md)).

## Hover

Resolves menu paths, properties, and verbs against the published reference
(not paraphrase). Menu cards list a bounded property set
(`MAX_HOVER_PROPERTIES`, descriptions capped at `MAX_HOVER_DESC_CHARS`).

## Diagnostics

Fixed severity ladder (source: `lsp/src/diagnostics.rs` header).
Semantic findings never escalate to Error; only syntax is Error:

| Severity | Rules |
| --- | --- |
| Error | unclosed/unmatched brace, unclosed quote (syntax only) |
| Warning | unknown menu path, unknown property, missing required arg (`add`/`set` on Directory menus), duplicate property, invalid enum value, unknown verb |
| Information | read-only column written as `key=`; truncation footer naming dropped findings |
| Hint | typed-value shapes (bool/num/time/MAC/IP plausibility), non-unsettable `unset` target — never blocking |

Work is bounded per document (`MAX_DIAG_LINES`, `MAX_DIAG_BYTES`,
`MAX_DIAGNOSTICS`); positions handle backslash continuations
(logical lines, physical positions).

## Symbols, signature, navigation, quickfixes

- **Symbols/outline:** menus + `:local`/`:global` variables; folding ranges
  (caps: `MAX_SYMBOLS`, `MAX_FOLDING_RANGES`).
- **Signature help:** required-first parameter hints on menu verbs
  (`MAX_SIGNATURE_PROPERTIES`).
- **Navigation:** go-to-definition / references between declarations
  and `$name` usages (`MAX_REFERENCES`).
- **Quickfixes:** `codeAction` offers edit-distance "did you mean …?"
  candidates (`MAX_CODE_ACTIONS`, `MAX_SUGGEST_INPUT_BYTES`).

Caps table: [lsp-config.md](lsp-config.md).
