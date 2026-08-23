# Skill: Zed Extension Development

## When to Use

Trigger this skill when the task involves any of:

- Editing `extension.toml` or the WASM shim (`src/lib.rs`)
- Debugging install failures (grammar 404, WASM build, LS not starting, diagnostics missing)
- Publishing to `zed-industries/extensions`
- Release automation (`release.yml`, version bumps, GitHub Release assets)

For grammar internals see `tree-sitter-grammar`; for LSP internals see `language-server`;
for command data see `commands-extraction`; for day-to-day commands see `development-workflow`.

## Project Identity

- **Extension ID:** `mikrotik-rsc` — **Name:** `MikroTik RouterOS Script`
- **Language:** `MikroTik Script` (Zed) / `RSC` (grammar) — **Suffix:** `.rsc`
- **Target:** RouterOS 7.22+
- **License:** Apache-2.0 — **Grammar repo:** [tree-sitter-rsc](https://github.com/balakar94/tree-sitter-rsc)

Hard constraints that govern this project are in [`AGENTS.md`](../../AGENTS.md) → *Hard rules*. Read them before editing.

## Extension Lifecycle

```
Install (Zed marketplace or Install Dev Extension)
  │
  ▼
Zed loads WASM (src/lib.rs: RscExtension::new)
  │
  ▼
language_server_command(id, worktree) called per worktree
  │
  ├─1 worktree.which("rsc-ls") ──found──► use PATH binary (dev: cargo build -p rsc-ls --release)
  ├─2 make_file_executable("rsc-ls") ──found──► reuse cached download (extension dir)
  └─3 auto-download:
       current_platform() → triple (aarch64-apple-darwin | x86_64-apple-darwin
                                   | x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu)
       set_language_server_installation_status(CheckingForUpdate → Downloading → None/Failed)
       latest_github_release() → github_release_by_tag_name("v<version>") → direct URL fallback
       download_file(url, "rsc-ls", Uncompressed)
       fetch(url + ".sha256") → sha256 verify
         ├─ match    → make_file_executable → cache
         └─ mismatch / bad companion / fetch error → FAIL CLOSED:
              delete downloaded binary + status Failed (never executed)
              │
              ▼
         rsc-ls --stdio JSON-RPC 2.0
           ├─ initialize → capabilities (completion, hover, diagnostic, sync Full)
           ├─ textDocument/didOpen|didChange → publishDiagnostics (5 rules, capped)
           ├─ textDocument/completion (triggers: /  space  =  :)
           ├─ textDocument/hover
           └─ textDocument/diagnostic (pull)
Windows → auto-download error; instruct manual build.
```

### Supply-chain verification (auto-download)

Downloads are checksum-verified before they are ever executed: after
`download_file`, the shim fetches the release's `<asset>.sha256` companion via
the Zed host HTTP client, hashes the downloaded bytes with a built-in pure-Rust
SHA-256 (`src/sha256.rs`, no dependencies), and compares digests. Any failure —
companion fetch error, unparseable companion, or hash mismatch — **fails
closed**: the binary is deleted, `LanguageServerInstallationStatus::Failed` is
set, and the binary is never run. Status/log messages show only 12-character
digest prefixes, never full hashes. Note: the PATH/cached-binary fast paths
(steps 1–3) are trusted as-is; only freshly downloaded artifacts are verified.

## extension.toml Format

```toml
id = "mikrotik-rsc"
name = "MikroTik RouterOS Script"
version = "0.1.0"  # bump together with Cargo.toml + lsp/Cargo.toml + grammars/rsc/*
[grammars.rsc]
repository = "https://github.com/balakar94/tree-sitter-rsc"
rev = "<full-commit-sha>"   # real SHA from the grammar repo — never 000..., never hand-edited
[language_servers.rsc-ls]
name = "MikroTik Language Server"
languages = ["MikroTik Script"]
```

## Tasks & Deploy

`scripts/mikrotik-deploy.py` pushes `.rsc` files over REST/SSH — full env-var reference in the
script docstring (`python scripts/mikrotik-deploy.py --help`). Always try `--dry-run` first.
Zed task templates: `languages/rsc/tasks.json` → copy to `.zed/tasks.json` to activate.

## Publishing Checklist

```bash
# 0. Ensure clean state
make validate   # generate-check + fmt + clippy + test-all + sync-check + extract

# 1. Publish grammar (if grammar.js changed)
python scripts/publish_grammar.py --dry-run
python scripts/publish_grammar.py --push  # pushes grammars/rsc → balakar94/tree-sitter-rsc
# verify extension.toml rev updated: git -C grammars/rsc rev-parse HEAD

# 2. Bump versions consistently
# edit: Cargo.toml, lsp/Cargo.toml, grammars/rsc/Cargo.toml, grammars/rsc/package.json, extension.toml
# keep semver; release.yml reads Cargo.toml version for tag fallback

# 3. Regenerate & commit data if llms-full.txt changed
python scripts/extract_commands.py && git diff --exit-code data/commands.toml

# 4. Tag → triggers .github/workflows/release.yml (multi-platform rsc-ls binaries + WASM)
git tag v<version> && git push origin v<version>
# artifacts: rsc-ls-<triple> + extension.wasm + *.sha256

# 5. PR to zed-industries/extensions
# fork zed-industries/extensions, add submodule: git submodule add https://github.com/balakar94/mikrotik-zed mikrotik-zed
# edit extensions.toml: ["mikrotik-rsc"] = { submodule = "mikrotik-zed", version = "x.y.z" }
# sort: pnpm sort-extensions  (requires Node 20)
# PR must pass: rev is real SHA, WASM builds, license present
```

## Common Errors

| Symptom | Cause | Fix |
|---------|-------|-----|
| `grammar not found` / `Failed to load grammar rsc` | `extension.toml` `rev` is placeholder `000...` or not pushed | `python scripts/publish_grammar.py --push`; verify `git ls-remote https://github.com/balakar94/tree-sitter-rsc <rev>` |
| `Failed to compile` WASM / `wasm32-wasip2` missing | Extension built without `wasm` target or uses `std::env`/`cfg` | `rustup target add wasm32-wasip2`; replace `std::env::var` with `worktree.shell_env()`; see `platform_triple()` in `src/lib.rs` |
| `404 download` / `Failed to download rsc-ls-<triple>` | No GitHub Release asset for version/triple; `CARGO_PKG_VERSION` mismatch | Tag and push matching `v<version>`; check `gh release view v<ver> --json assets`; fallback: `cargo build -p rsc-ls --release && export PATH=$PATH:target/release` |
| Diagnostics not showing | File not `language: MikroTik Script` (`.rsc` suffix or `config.toml` mismatch); doc exceeds caps; invalid URI | Check `languages/rsc/config.toml` suffix; `zed: open log` for `[rsc-ls]`; open via real `file://` path; check caps in `diagnostics.rs` |
| `Windows auto-download not supported` | `platform_triple()` rejects Windows | Build manually: `cargo build -p rsc-ls --release` and add to PATH |
| `stale parser.c` CI failure | Edited `grammar.js` without regenerating | `cd grammars/rsc && npx tree-sitter generate && git add src/parser.c src/grammar.json src/node-types.json` |
| `data/commands.toml stale` CI failure | Edited `llms-full.txt` without re-extracting | `python scripts/extract_commands.py && git add data/commands.toml` |
| Tasks not appearing | `.zed/tasks.json` missing or not valid JSON | `cp languages/rsc/tasks.json .zed/tasks.json`; validate JSON; restart Zed |

## Reference

- Zed extension docs: https://zed.dev/docs/extensions/developing-extensions — languages: https://zed.dev/docs/extensions/languages
- `zed_extension_api`: https://docs.rs/crate/zed_extension_api/latest
- Registry: https://github.com/zed-industries/extensions
