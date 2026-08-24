# Skill: Tree-sitter Grammar for RSC

## When to Use

Trigger this skill when:

- Editing `grammars/rsc/grammar.js` or any `languages/rsc/*.scm` query file
- Adding a new RSC syntax construct, keyword, literal, control-flow form, or operator
- Fixing highlighting, indentation, bracket matching, or outline/symbol issues
- Regenerating `src/parser.c` / `src/grammar.json` / `src/node-types.json`
- Adding or updating corpus tests (`grammars/rsc/test/corpus/*.txt`) or fixtures (`test/*.rsc`)
- Debugging parse errors (`tree-sitter parse/highlight/query`) or Zed highlighting failures
- Bumping `tree-sitter-cli`, `TREE_SITTER_LANGUAGE_VERSION`, or publishing the grammar repo

## Grammar Location

- **Source:** `grammars/rsc/grammar.js` — own repo checked out as an **untracked working copy** (`https://github.com/balakar94/tree-sitter-rsc`; pinned rev in `extension.toml` → `[grammars.rsc] rev`). Fresh clone: `make grammar-clone`
- **Generated:** `grammars/rsc/src/parser.c` (`TREE_SITTER_LANGUAGE_VERSION 15`), `src/grammar.json`, `src/node-types.json` — never edit by hand
- **Metadata:** `grammars/rsc/tree-sitter.json` (scope `source.rsc`, file-types `["rsc"]`); crate + CLI versions live in `Cargo.toml` / `package.json` — read them there, don't trust remembered numbers
- **Queries:** canonical `languages/rsc/*.scm` (used by Zed) → deduped copy `grammars/rsc/queries/highlights.scm` (used by `tree-sitter test` / playground)
- **Tests:** `grammars/rsc/test/corpus/*.txt` (run `npx tree-sitter test` for current count), `test/simple.rsc` (smoke fixture), `test/example.rsc` (rich integration)
- **Zed config:** `languages/rsc/config.toml` (name `RSC`, suffixes `rsc`, brackets, word chars `_- $`)

## Grammar Architecture

### Top-Level Structure

```
source_file → optional(_statement) · repeat(_terminated_statement) · optional(separator)
_terminated_statement → _statement_separator (";" | "\n") · _statement
_statement → menu_command (prec 2) | menu_continuation (prec 1) | global_command | _value | parent_navigation ("..")
```

`extras: [ /[ \t]+/, /\r/, $.comment ]` — spaces/tabs are extras; `\n` and `;` are significant separators. `word: $.identifier`.

### Statement Types

| Node                   | Syntax                                    | Example                                                   |
| ---------------------- | ----------------------------------------- | --------------------------------------------------------- |
| `menu_command`         | `/path params…` (prec 2)                  | `/ip address add address=192.168.1.1/24 interface=ether1` |
| `menu_continuation`    | `params…` without `/` (prec 1, `repeat1`) | `    address=10.0.0.1/24 interface=ether1`                |
| `root_menu`            | First ident after `/`                     | `ip` in `/ip route`                                       |
| `sub_menu`             | Subsequent idents (prec 1)                | `route`, `add`                                            |
| `global_command`       | `:name body? params…` (prec 1)            | `:if (cond) do={ … }`                                     |
| `named_param`          | `key=value` (value optional)              | `name=ether1`, `disabled=`                                |
| `block`                | `{ … }`                                   | `do={ :put "hi" }`                                        |
| `command_substitution` | `[stmt]`                                  | `[find where name=ether1]`                                |
| `variable_reference`   | `$name`                                   | `$myVar`                                                  |
| `array`                | `{ elem; … }`                             | `{1;2;3}` / `{a=1;b=2}`                                   |

### Control Flow (via `global_command`)

```rsc
:if (condition) do={ ... } else={ ... }
:while (condition) do={ ... }
:foreach i in=[find] do={ ... }
:do { ... } while=(condition)
:for i from=0 to=10 step=1 do={ ... }
```

Parsed as `global_command` + `_command_body` (`do_block`/`else_block`/`while_condition`/`for_in_clause`).

### Key Grammar Details

- `identifier: /[a-zA-Z_][a-zA-Z0-9_@-]*/` — `@` for emails (`user@domain`), `-` for `caps-man`/`force-update`; `ether1`, `bridge1` valid
- `line_continuation: token(seq("\\", "\n"))` — backslash+newline as single token; allowed inside `menu_command`/`menu_continuation` via `repeat(choice(named_param, line_continuation, _value))`
- `conflicts: [[subexpression, _value], [menu_continuation, _statement], [named_param]]` — GLR for `(…)`, indented continuation vs bare value, and `key=` empty-value ambiguity
- `_value → literal | variable_reference | command_substitution | subexpression | array | array_access | function_call | identifier | operator`
- Literals: `number` (`0x` hex + decimal), `string` (`"…"`, `\\` escapes), `boolean_literal` (`true`/`false`/`yes`/`no`), `nil_literal` (`nil`), `ip_address`, `ip_prefix`
- `TREE_SITTER_LANGUAGE_VERSION 15` in `src/parser.c:9` — required by `zed_extension_api` WASM build

## Grammar Evolution

Historical rule-by-rule changes live in the submodule's git log (`git -C grammars/rsc log`) — not duplicated here. When you change the grammar in a non-obvious way, capture _why_ in the commit message, not in this skill.

## Making Changes — 6-Step Workflow

1. **Edit `grammar.js`** — keep `extras`/`word`/`conflicts` minimal; add new rule under `rules:` and wire into `_statement` or parent node. Use `prec` to disambiguate. Preserve `identifier` charset unless new char required.
2. **Regenerate** — `cd grammars/rsc && npx tree-sitter generate` (or `npm run generate`). Verify `src/parser.c:9` is `LANGUAGE_VERSION 15` and `src/grammar.json`/`src/node-types.json` diff looks expected.
3. **Corpus tests** — `npx tree-sitter test` (or `npm test`) must pass clean. Parse fixtures: `npx tree-sitter parse test/simple.rsc` and `npx tree-sitter parse test/example.rsc` — no `ERROR`/`MISSING`.
4. **Update queries** — edit canonical `languages/rsc/*.scm` first, then dedup: `cp languages/rsc/highlights.scm grammars/rsc/queries/highlights.scm`. Verify order-sensitive highlights (see below).
5. **Verify rendering** — `npx tree-sitter highlight test/example.rsc` (ANSI) and `npx tree-sitter highlight test/example.rsc --html` (exact captures); check brackets/indents with real Zed via `Install Dev Extension`.
6. **Publish** — `python scripts/publish_grammar.py --dry-run` then `python scripts/publish_grammar.py --push` (pushes `grammars/rsc` to `balakar94/tree-sitter-rsc` and updates `extension.toml` rev). Requires `tree-sitter generate` clean and corpus green.

Verification:

```bash
npx tree-sitter generate && npx tree-sitter test                    # all pass
npx tree-sitter parse test/example.rsc | grep -q ERROR && echo FAIL || echo OK
npx tree-sitter highlight test/example.rsc --html | head -20        # HTML captures
make validate   # also runs extract_commands.py + cargo fmt/clippy
```

### Common Recipes

- **Add a keyword group** (e.g. new verb): add `(#match? @keyword "^(new-verb)$")` in `languages/rsc/highlights.scm` after the catch-all, before keyword overrides — no grammar change needed if already `identifier`.
- **Add a new literal** (e.g. time duration `00:01:00`): add token rule in `grammar.js` `rules:` → add to `literal` choice → `npx tree-sitter generate` → add corpus test → add `(new_literal) @number` in `highlights.scm`.
- **Add a new node type**: define rule, reference from `_statement` or `_value`, set `prec` if ambiguous, add `conflicts` entry if GLR, update queries (highlights/brackets/indents/outline), add corpus.

## Query File Maintenance

**Canonical source is `languages/rsc/`** (Zed loads at runtime). Edit there first, then dedup. Grammar repo queries are for `tree-sitter test` and WASM playground only.

```scheme
; highlights.scm — order matters (first match wins), specific → catch-all → overrides
(menu_prefix) @punctuation.special
(root_menu (identifier) @function)          ; blue
(sub_menu (identifier) @string)             ; green
(command_substitution (identifier) @string) ; green inside [...]
(menu_continuation (identifier) @string)    ; green continuation
(menu_command (identifier) @constant)       ; orange catch-all — AFTER string, BEFORE keyword
((sub_menu (identifier) @keyword) (#match? @keyword "^(add|remove|set|…|ping)$")) ; purple LAST
(named_param name: (identifier) @type) "=" @operator value: (identifier) @constant
(global_command_name) @keyword ; :put, :if, :local …
(string) @string.special ; (number)/(ip_address) @number ; (comment) @comment
```

- **Indents** (`indents.scm`): modern Zed contract — one `@indent` per bracketing node (`(block) @indent`, same for `command_substitution`/`subexpression`). Single-line matches are ignored; legacy `@indent.begin/@end/@continue` names are gone.
- **Brackets** (`brackets.scm`): `("(" @open ")" @close)` triple for `()`, `[]`, `{}` — must match grammar delimiters.
- **Outline** (`outline.scm`): `menu_command (root_menu @context) (sub_menu @name) @item` → `ip > address` in symbol view.
- **Injections**: none — an empty placeholder file fails Zed's validation, so no `injections.scm` exists.

Always verify captures after reordering: `npx tree-sitter highlight test/example.rsc --html | grep -c keyword` should include verbs.

## Corpus Management

- **Format** (`test/corpus/*.txt`): Each test is a block (see any file, e.g. `menu_commands.txt`):
  ```
  ==========
  Test name
  ==========
  /ip route print
  ---
  (source_file (menu_command (menu_prefix) (root_menu (identifier)) …))
  ```
- **Files:** `basics`, `menu_commands`, `global_commands`, `named_params`, `blocks`, `variables`, `arrays`, `strings`, `literals`, `subexpressions`, `command_substitution`, `operators_nav`, `separators`, `line_continuation` (empty placeholder).
- **Add a test:** append a block to the relevant file, run `npx tree-sitter test --filter "Test name"` to preview, then regenerate expected trees with `npx tree-sitter test -u` (`--update` rewrites the sexp from current parser output — verify the diff before committing). Run full `npx tree-sitter test` to confirm all pass.
- **Fixtures:** `test/simple.rsc` (`/ip route print`) for smoke parse; `test/example.rsc` covers vars, control flow, menus, arrays, `\` continuations, `..` navigation.
- **Scripts:** `npm test` → `tree-sitter test`; `npm run generate` → `tree-sitter generate`; `npm run playground` → `build-wasm && playground` at `http://localhost:8000`.
- **Update sexp:** prefer `npx tree-sitter test -u`. Only hand-paste from `npx tree-sitter parse <file>` output if you need selective control over which trees change.

## Debugging

```bash
cd grammars/rsc
npx tree-sitter parse test/example.rsc              # sexp, look for ERROR/MISSING
npx tree-sitter parse --debug test/example.rsc      # GLR trace for conflicts
npx tree-sitter highlight test/example.rsc          # ANSI (checks query order)
npx tree-sitter highlight test/example.rsc --html   # HTML for exact capture debugging
npx tree-sitter query "(menu_command) @m" test/example.rsc  # ad-hoc probe
npx tree-sitter test --filter "Simple menu"         # single corpus test
```

- **Zed logs:** `zed --foreground` (stderr stream) or palette `zed: open log` → filter `rsc`/`tree-sitter`/`language`. After grammar change: `Install Dev Extension` → select `mikrotik-zed/` → check log for `Loaded language RSC` and `Loaded grammar rsc`. If highlighting stale, reload window or `cargo build --target wasm32-wasip2`.
- **Highlight debugging:** compare `npx tree-sitter highlight` vs Zed — if CLI highlights correctly but Zed does not, check `languages/rsc/config.toml` scope and `extension.toml` grammar rev. Zed caches WASM — `rm -rf ~/.config/zed/extensions/mikrotik-rsc` after `tree-sitter build-wasm`.
- **Parse debugging:** `ERROR` nodes mean no rule matched — check `extras` (is `\n` consumed as extra?), `prec` ordering (`menu_command` prec 2 must beat `menu_continuation` prec 1), and `conflicts` (add single-element `[named_param]` for optional value). `MISSING` after `:` → `global_command_name` is `seq(":", identifier)`.
- **Pitfalls:** highlights wrong → verb `@keyword` must be LAST after catch-all; `\` broken → `line_continuation` must be `token(seq("\\","\n"))` not `"\\"`; indented export errors → `menu_continuation` missing or not in `_statement`; `key=` flagged → `named_param` value must be `optional`; duplication drift → `diff languages/rsc/highlights.scm grammars/rsc/queries/highlights.scm` must be empty.
