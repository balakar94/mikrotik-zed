# Glossary

Terms used by the extension, the editor, and RouterOS. The goal is one
word per concept in user-facing copy.

| Term | Meaning | Where you see it |
| --- | --- | --- |
| **Path** | A slash-separated RouterOS location, e.g. `/ip/address`. Written with or without spaces (`/ip address`). Case-insensitive. | Editor text, diagnostics |
| **Menu** | A path that holds entries and/or sub-menus. `Directory` in the dataset means "menu with entries". | Hover `Type:`, completion detail |
| **Verb** | An action applied on a menu, e.g. `add`, `set`, `print`, `remove`. RouterOS console documentation also calls these "commands". | Completion, diagnostics (`Unknown command '…'`), hover |
| **Property** | A `name=value` argument of a verb, e.g. `address=`, `chain=`. Documentation may call them "arguments". | Completion (required first), diagnostics |
| **Value** | What goes after `=`: an enum member, a boolean, a number, a live device name, … | Value completion |
| **Required property** | A property the device will reject the command without, for `add`/`set` on menus. | Warning diagnostic, hover `(required)` |
| **Flag** | A single-letter modifier (e.g. `!` prefixes on print output); offered below properties in completion. | Completion tier 7 |
| **Live enrichment** | Opt-in device data (interfaces, addresses, lists, chains, pools) merged into value completion, marked `live — …`. | Completion detail, logs |
| **Dataset** | The embedded command table generated from MikroTik's CLI reference (`data/commands.toml`). | Hover `Source:` line |
| **rsc-ls** | The language server binary that powers completion, hover, and diagnostics. | Logs, configuration |
| **Shim** | The small WASM component Zed loads; it only resolves and starts `rsc-ls`. | Install/troubleshooting |
| **REST** | RouterOS JSON API used by live enrichment and deploy. Enabled per service and user policy. | Prerequisites, deploy |
| **TOFU** | Trust-on-first-use for SSH host keys; the accepted fingerprint is printed so it can be verified. | Deploy SSH |
| **Sentinel** | The `RSC_DEPLOY_OK` marker appended to REST-executed scripts; its absence means the result is unverified. | Deploy verification |

## Dataset types in hover cards

Hover renders the dataset's own type verbatim:

- `Type: Directory` — a menu that lists entries and accepts verbs.
- `Type: Settings Directory` — a singleton settings menu.
- `Type: Command` — an action entry under a path rather than a property.

Prefer the plain-language column above when explaining these to non-experts.

## Not to be confused with

- **Script vs config** — RouterOS calls a `.rsc` file a script; `/export`
  output is the same syntax but describes configuration. The editor treats
  both identically.
- **Settings vs workspace settings** — Zed's `lsp.rsc-ls.binary.env`
  reaches the server; `lsp.rsc-ls.settings` does not (see
  [Configuration](configuration.md#how-settings-reach-the-language-server)).
