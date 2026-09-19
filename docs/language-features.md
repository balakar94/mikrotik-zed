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
| Verb | `2` | `add`, `set`, … accepted by the menu; script globals (`:local`) share this tier |
| Sub-menu | `3` | child paths in context |
| Enum value | `4` | documented enum/bool members |
| Common hint | `5` | curated values (e.g. firewall chains), one tier below true members |
| Placeholder | `6` | honest type placeholder (`0.0.0.0/0`) when nothing better exists |
| Flag | `7` | single-letter flags |
| Demoted | `8` | typo fallback when the prefix matched nothing |
| Snippet | `9` | statement snippets, single shared key (curated order) |

Within a tier, match quality orders: exact < prefix < substring.
Responses are capped (`MAX_COMPLETION_ITEMS` — see [caps](lsp-config.md)).
Property completion skips keys already present on the line, and string
properties insert their quotes for you.

### Script commands (`:`)

Typing `:` offers the curated RouterOS script globals — `:if`, `:foreach`,
`:for`, `:do`, `:local`, `:global`, `:put`, `:return`, `:error`, `:delay`,
`:resolve`, `:parse`, `:pick`, `:tonum`, `:totime` — with one-line
documentation. The list is shared with hover, so the popup and the card can
never disagree. Structural statements (`:if`, `:foreach`, `:for`, `:do`) keep
their richer multi-line snippets, indented with four spaces, and win over the
plain entry for the same label.

## Hover

Resolves menu paths, properties, and verbs against the published reference
(not paraphrase). Every card ends with a source line naming the dataset's
RouterOS version: `Source: published reference — RouterOS <version>`.

Menu cards list required entries first, then optional ones. The cap
(`MAX_HOVER_PROPERTIES`) applies **per section** — arguments (required plus
optional), flags, and read-only columns each have their own budget — and each
section's footer says what it hid: completion can list properties and flags,
while read-only columns are summarised without promising a completion list.
Menu cards render the dataset type verbatim (`Type: Directory`,
`Type: Command`); see the [glossary](glossary.md#dataset-types-in-hover-cards).

Property cards show type plus a plain-language gloss (`iface` → "interface
name — from device (Live) or type manually"), the owning command
(`in \`/ip/address add\``), enum values, an example where useful, and the
source line. Descriptions longer than `MAX_HOVER_DESC_CHARS` are cut and
marked `(truncated)`.

## Diagnostics

Fixed severity ladder (source: `lsp/src/diagnostics.rs` header).
Semantic findings never escalate to Error; only syntax is Error:

| Severity | Rules |
| --- | --- |
| Error | unclosed/unmatched brace, unclosed quote (syntax only) |
| Warning | unknown menu path, unknown property, missing required arg (`add`/`set` on menu-like paths), duplicate property, invalid enum value, unknown verb |
| Information | read-only column written as `key=`; truncation footer naming dropped findings |
| Hint | typed-value shapes (bool/num/time/MAC/IP plausibility), non-unsettable `unset` target — never blocking |

Messages follow `lead verb + target + fix` (for example
`Missing required 'address=' for 'add' on '/ip/address' — add address=...`).
The word "command" in a diagnostic means the verb you typed; see the
[glossary](glossary.md).

Work is bounded per document (`MAX_DIAG_LINES`, `MAX_DIAG_BYTES`,
`MAX_DIAGNOSTICS`); positions handle backslash continuations
(logical lines, physical positions). Deploy treats Warning-level semantic
findings as real: many of them break `/import` on the device.

## Symbols, signature, navigation, quickfixes

- **Symbols/outline:** menus + `:local`/`:global` variables; folding ranges
  (caps: `MAX_SYMBOLS`, `MAX_FOLDING_RANGES`).
- **Signature help:** required-first parameter hints on menu verbs
  (`MAX_SIGNATURE_PROPERTIES`).
- **Navigation:** go-to-definition / references / rename between declarations
  and `$name` usages (`MAX_REFERENCES`).
- **Quickfixes:** `codeAction` offers edit-distance "did you mean …?"
  candidates (`MAX_CODE_ACTIONS`, `MAX_SUGGEST_INPUT_BYTES`).

Caps table: [lsp-config.md](lsp-config.md).
