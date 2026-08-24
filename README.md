<h1 align="center">MikroTik RouterOS Script — Zed Extension</h1>

<p align="center">
  Complete Zed integration for MikroTik RouterOS Script<br>
  <em>Tree-sitter highlighting · LSP completion & hover · Diagnostics · Deploy</em>
</p>

<p align="center">
  <a href="https://github.com/balakar94/mikrotik-zed/releases"><img src="https://img.shields.io/github/v/release/balakar94/mikrotik-zed?label=release&color=blue" alt="release"></a>
  <a href="https://github.com/balakar94/mikrotik-zed/actions/workflows/ci.yml"><img src="https://github.com/balakar94/mikrotik-zed/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/balakar94/mikrotik-zed/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-green" alt="license"></a>
  <a href="https://manual.mikrotik.com/docs/cli-reference/"><img src="https://img.shields.io/badge/RouterOS-v7.0%2B-red" alt="RouterOS"></a>
  <a href="https://zed.dev"><img src="https://img.shields.io/badge/Zed-extension-black" alt="Zed"></a>
  <a href="https://github.com/balakar94/tree-sitter-rsc"><img src="https://img.shields.io/badge/tree--sitter-rsc-orange" alt="grammar"></a>
</p>

---

## ✨ Features

| Area                  | What you get                                                                                                                                                                                                                   |
| --------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **Highlighting**      | Tree-sitter grammar (`grammars/rsc/`) — menus, globals, control flow, variables, arrays, menu continuation                                                                                                                     |
| **Completion**        | Root menus, sub-menus, verbs, properties with snippets (`address=$1`), enum/bool values with embedded docs, `:` script words + statement snippets (`:if`, `:foreach`, `:for`, `:do`) — `triggerCharacters: / space = :`        |
| **Hover**             | Menu docs (`Type` + `Arguments` + `Flags`), property types, standard verbs                                                                                                                                                     |
| **Diagnostics**       | Menu semantics (`unknown-menu` · `unknown-property` · `missing-required` · `duplicate-property` · `invalid-enum-value`) and syntax errors (`unclosed-brace` · `unmatched-brace` · `unclosed-quote`) — capped, `source: rsc-ls` |
| **Outline & folding** | Document symbols (menus + `:local`/`:global` variables), folding ranges (braces + `\` continuations)                                                                                                                           |
| **Signature help**    | Required-first parameter popup for menu verbs                                                                                                                                                                                  |
| **Navigation**        | Go-to-definition / find references for script variables (`:local`/`:global` ↔ `$name`)                                                                                                                                         |
| **Quick fixes**       | "Did you mean …?" for typos in properties, menus, and enum values                                                                                                                                                              |
| **Tasks & Deploy**    | Zed tasks (`REST`/`SSH`/`Dry-run`/`Validate`) + `scripts/mikrotik-deploy.py` (`requests`/`paramiko`)                                                                                                                           |
| **Sync**              | `scripts/sync_llms.py` fetches latest `llms-full.txt` from `manual.mikrotik.com`                                                                                                                                               |
| **Grammar**           | In-tree + published to [`balakar94/tree-sitter-rsc`](https://github.com/balakar94/tree-sitter-rsc) (rev pinned in `extension.toml`)                                                                                            |

**Coverage:** **1038 menus** — complete CLI from `llms-full.txt` (491 Directory + 432 Command + implicit parents). All roots: `/interface`, `/ip`, `/ipv6`, `/routing`, `/queue`, `/system`, `/tool`, `/user`, `/certificate`, `/caps-man`, `/container`, `/disk`, `/file`, `/ppp`, `/mpls`, `/radius`, …

<details>
<summary>Example <code>.rsc</code> — hover, completion, diagnostics</summary>

```rsc
# /ip/address — hover shows Type + Arguments
/ip address add address=10.0.0.1/24 interface=ether1

# completion after `chain=` → input | forward | output
/ip firewall filter add chain=input action=accept comment="allow"

# diagnostics: missing required `address` → Info
/ip address add interface=ether1

# multi-line with backslash continuation
/tool fetch url="https://example.com/very/long/url/that/needs/continuation" \
    mode=https

# control flow
:if ($var > 10) do={ :put "ok" } else={ :error "fail" }
```

</details>

---

## 🔧 Dependencies & Bootstrap

| Requirement                  | Required          | Purpose                                                                                      |
| ---------------------------- | ----------------- | -------------------------------------------------------------------------------------------- |
| Rust 1.90+ (MSRV)            | ✅ required       | Builds `rsc-ls` + WASM extension; pinned via `rust-toolchain.toml`                           |
| `wasm32-wasip2` target       | ✅ required       | WASM extension build (`rustup target add wasm32-wasip2`)                                     |
| C compiler + linker          | ✅ required       | Native Rust builds (Xcode CLT / `build-essential` / `base-devel`)                            |
| git                          | ✅ required       | Submodule checkout (`grammars/rsc`)                                                          |
| curl                         | ✅ required       | Doc sync, rustup installer, LSP auto-download                                                |
| CA certificates              | ✅ required       | TLS for curl / pip / npm                                                                     |
| Python 3.12+                 | ✅ required       | Extraction/sync scripts, tests                                                               |
| pytest + requests + paramiko | ✅ auto-installed | Into `.venv` by `make install-tools`                                                         |
| Node.js LTS + npm            | ⚠️ optional       | Grammar work only (`tree-sitter-cli` via `npx`)                                              |
| wasm-tools, cargo-audit      | ⚪ optional       | `make build-wasm` validation/legacy fallback (rustc already emits a component), `make audit` |

### One-liner bootstrap

```bash
make install
```

Detects your platform and bootstraps everything: system packages (**macOS/Homebrew**, **Fedora/RHEL** `dnf`/`yum`, **Arch** `pacman`, **Debian/Ubuntu** `apt`), rustup + `wasm32-wasip2`, the `.venv` with pytest/requests/paramiko, local `tree-sitter-cli`, then builds and installs `rsc-ls`. Idempotent — safe to re-run.

- Skip distro packages (containers, CI, no sudo): `SKIP_SYSTEM=1 make install`
- Granular targets:
  - `make install-deps` — system packages only
  - `make install-tools` — rustup + `.venv` + npm deps
  - `make install-lsp` — build + copy `rsc-ls` to PATH

### Manual fallback (per platform)

macOS (Homebrew):

```bash
xcode-select --install
brew install python node
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
```

Fedora/RHEL:

```bash
sudo dnf install -y gcc gcc-c++ make curl git ca-certificates openssl-devel pkgconf-pkg-config python3 python3-pip nodejs npm
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
```

Debian/Ubuntu:

```bash
sudo apt-get update
sudo apt-get install -y build-essential curl git ca-certificates pkg-config libssl-dev python3 python3-venv python3-pip nodejs npm
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
```

Arch:

```bash
sudo pacman -Sy --needed --noconfirm base-devel curl git python python-pip nodejs npm
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
```

Common tail (all platforms):

```bash
rustup target add wasm32-wasip2
make install-tools
cd grammars/rsc && npm install
```

> `make install-tools` uses a project-local `.venv` instead of system pip, which sidesteps PEP 668 on Fedora/Debian/Arch.

---

## 📦 Install

### From Zed Extensions (once published)

Zed → `zed: extensions` → search **MikroTik** → **Install**.

### Dev install (local)

```bash
# 1. Optional: build LSP locally (otherwise extension auto-downloads from Releases)
cargo build -p rsc-ls --release
export PATH="$PWD/target/release:$PATH"

# For GUI Zed (Dock), also copy to a GUI-visible PATH (or just run: make install-lsp):
cp target/release/rsc-ls ~/.cargo/bin/rsc-ls
cp target/release/rsc-ls /opt/homebrew/bin/rsc-ls  # macOS

# 2. Zed → Command Palette → Install Dev Extension → select this directory
# 3. Open a .rsc file; check logs via zed --foreground or zed: open log
```

### Binary auto-download

`src/lib.rs` (WASM, `zed_extension_api 0.7`) resolves `rsc-ls` as:

1. `worktree.which("rsc-ls")` — PATH override (dev; on Windows `rsc-ls.exe` is probed too)
2. Cached binary from prior download (extension dir)
3. **Auto-download** from GitHub Releases via `current_platform()` → `rsc-ls-<triple>`:

| Triple                      | Platform            |
| --------------------------- | ------------------- |
| `aarch64-apple-darwin`      | macOS Apple Silicon |
| `x86_64-apple-darwin`       | macOS Intel         |
| `aarch64-unknown-linux-gnu` | Linux ARM64         |
| `x86_64-unknown-linux-gnu`  | Linux x64           |
| `x86_64-pc-windows-msvc`    | Windows x64         |
| `aarch64-pc-windows-msvc`   | Windows ARM64       |

Uses `zed::download_file` + `make_file_executable` with `LanguageServerInstallationStatus` UI. On 404, shows manual install instructions.

> **Windows note:** the binary is downloaded, cached, and spawned as `rsc-ls.exe` there — Windows cannot execute an executable image whose file lacks the `.exe` suffix. Release assets themselves stay extension-less byte blobs (`rsc-ls-x86_64-pc-windows-msvc`), so one asset scheme covers all platforms.

**Supply-chain check:** every download is SHA-256–verified against its `<asset>.sha256` release companion _before_ it is made executable or run. Any mismatch (or missing companion) fails the install and deletes the unverified binary — fail closed, never execute unverified.

> **404 after fresh Release?** GitHub CDN takes ~1-2 min to propagate `releases/download`. Use the PATH copy above meanwhile; `gh release download` via API works immediately.

---

## 🚀 Quick start

```bash
# Try completion / hover / diagnostics
cat > /tmp/demo.rsc <<'RSC'
/ip address add address=10.0.0.1/24 interface=ether1
/ip firewall filter add chain=input action=accept
/certificate add name=my-cert common-name=example.com
RSC
# open in Zed
open -a Zed /tmp/demo.rsc
```

- **Trigger completion:** type `/`, ` `, `=`, or `:` (script words like `:if`) — the server pre-filters; Zed fuzzy-filters on top
- **Hover:** over `/ip/address` or `chain`
- **Diagnostics:** delete `address=` → `Info: missing-required`

---

## 🛫 Tasks — Deploy to MikroTik

Zed tasks are per-worktree (`.zed/tasks.json`). Shipped:

- `languages/rsc/tasks.json` — template
- `.zed/tasks.json` — active (copy of template)
- `scripts/mikrotik-deploy.py` — deploy companion

### Companion script

```bash
python scripts/mikrotik-deploy.py --help

# Env (also CLI flags):
export MIKROTIK_HOST=192.168.88.1
export MIKROTIK_USER=admin
export MIKROTIK_PASS=secret
# optional:
export MIKROTIK_PORT=443
export MIKROTIK_METHOD=rest   # rest | ssh | auto
export MIKROTIK_SSL=0         # 0 = disable SSL verify (does NOT change scheme)
export MIKROTIK_HTTP=1        # 1 = force plain http:// (default https://)
export MIKROTIK_TIMEOUT=60    # seconds to wait for SSH /import (default 60)
export MIKROTIK_ACCEPT_HOST_KEY=1   # SSH only: trust unknown host keys (TOFU); MITM risk

# Dry-run (no device needed):
python scripts/mikrotik-deploy.py file.rsc --dry-run
MIKROTIK_HOST=192.168.88.1 python scripts/mikrotik-deploy.py file.rsc --dry-run

# REST (RouterOS 7+):
python scripts/mikrotik-deploy.py file.rsc --host 192.168.88.1 --user admin
python scripts/mikrotik-deploy.py file.rsc --host 192.168.88.1 --http --dry-run  # plain HTTP
# SSH:
python scripts/mikrotik-deploy.py file.rsc --method ssh --host 192.168.88.1

# Deps (optional transports):
pip install requests paramiko
# Auto-detects: missing requests → falls back to paramiko
```

**How it works:** REST `POST /rest/execute` (fallback `/rest/file` + `/import`); SSH `SFTP` + `/import`. Import output is scanned for high-confidence failure markers (`syntax error`, `input does not match`, `bad command name`, `failure:`) — HTTP 200 / SSH 0 does not guarantee success. Scheme is `https` by default; `--http` is explicit, `--no-ssl-verify` only disables cert validation (legacy shim warns if it previously implied `http`).

### Zed tasks

```bash
mkdir -p .zed && cp languages/rsc/tasks.json .zed/tasks.json
# then: Zed → task: spawn → MikroTik: Deploy current file (REST|SSH|Dry-run|Validate)
# Tasks use $ZED_FILE and $ZED_WORKTREE_ROOT, output to terminal panel
```

---

## 🧠 Language Server

**Pure Rust, no Node.** `lsp/` embeds `data/commands.toml` (1038 menus) via `include_str!()` — 555 Rust tests.

- `menus.rs` — loads TOML, validates paths (`..`, control chars, charset, length ≤256), builds `menu_by_path` + `child_names_by_parent` (implicit parents)
- `completion.rs` — `compute_completions(before_cursor)` — root / sub-menu / verb / arg / value (enums, bools, IP placeholders), statement snippets, `:`-triggered script-word filtering
- `hover.rs` — `compute_hover(line, character, full_doc, cursor_line)` — UTF-8 safe, `find_word_start/end`
- `diagnostics.rs` — 5 semantic rules (`unknown-menu`/`unknown-property`/`missing-required`/`duplicate-property`/`invalid-enum-value`) + 3 syntax rules (`unclosed-brace`/`unmatched-brace`/`unclosed-quote`), `MAX_DIAG_LINES 3000` / `MAX_DIAG_BYTES 500KB`, handles incremental edits + RouterOS backslash line continuation (`\` → logical lines, ranges mapped to physical lines)
- `symbols.rs` / `folding.rs` — document symbols (menus + script variables) and folding ranges
- `signature.rs` / `suggest.rs` — signature help; did-you-mean candidates behind quick fixes
- `navigation.rs` — go-to-definition / find-references for script variables

Protocol: `stdio` JSON-RPC 2.0, `Content-Length` framing, `MAX_MESSAGE_SIZE 10MiB`, `MAX_HEADER_SIZE 32KiB`, `MAX_DOCS 100`, `MAX_DOC_SIZE 5MiB`, `file://` URI validation (rejects `..`, `\0`, non-`file://`).

Capabilities: incremental sync (`change: 2`), completion (`triggerCharacters: / space = :`), hover, document symbols, folding ranges, signature help, code actions (quick fixes), definition, references, pull diagnostics.

---

## 🌳 Grammar

Git submodule at `grammars/rsc/` ([balakar94/tree-sitter-rsc](https://github.com/balakar94/tree-sitter-rsc)).
After cloning this repo: `git submodule update --init`.

```bash
cd grammars/rsc
npx tree-sitter generate          # grammar.js → src/parser.c
npx tree-sitter test              # 67 corpus tests
npx tree-sitter parse test/example.rsc
npx tree-sitter highlight test/example.rsc
```

Queries: `languages/rsc/highlights.scm` is canonical; `grammars/rsc/queries/highlights.scm` is a deduped copy for `tree-sitter test`.

Publishing:

```bash
python scripts/publish_grammar.py --dry-run
python scripts/publish_grammar.py --push  # pushes + updates extension.toml rev
```

---

## 🔄 Sync & Extraction

```bash
python scripts/sync_llms.py --check   # exit 2 if upstream changed
python scripts/sync_llms.py           # fetch https://manual.mikrotik.com/llms*.txt
python scripts/extract_commands.py    # → data/commands.toml (1038 menus)
# verify:
rg -c '^\[\[menus\]\]' data/commands.toml
rg 'path = "/ip/firewall/filter"' data/commands.toml
```

Header in `commands.toml` includes `RouterOS version`, `Generated` UTC, `Source hash` for traceability. `make sync-check` is CI-gated; `make extract` is idempotent — skips anonymous `ArgTableRow` with empty `arg`.

---

## 🛠️ Development

```bash
make help
make generate          # parser.c (tree-sitter)
make test-all          # 835 tests: grammar (67) + rust (555) + python (213)
make extract           # commands.toml (1038 menus)
make build             # WASM (Zed builds as component on install)
make build-lsp         # native rsc-ls
make check             # cargo check wasm (wasip2) + lsp
make fmt clippy audit
make validate          # generate-check + fmt + clippy + test-all + sync-check + extract
```

Health checks (mirrors CI):

```bash
cargo check --target wasm32-wasip2
cargo check -p rsc-ls
cargo fmt --all -- --check
cargo clippy --target wasm32-wasip2 -- -D warnings
cargo clippy -p rsc-ls -- -D warnings
cargo test -p rsc-ls                 # 555 tests (537 unit + 4 cli + 14 e2e)
python -m pytest tests/ -v           # 213 tests
npx tree-sitter test                 # 67 tests (run inside grammars/rsc/)
make generate-check
make validate
```

**Observability:** LSP logs to **stderr** (stdout is LSP):

```bash
RSC_LS_LOG=debug zed --foreground      # or: RUST_LOG
# levels: error < warn < info < debug < trace (default info)
# prefixes: [rsc-ls][LEVEL] and [mikrotik-zed]
```

---

## 📤 Release

- Grammar: `scripts/publish_grammar.py --push`
- Extension + LS: `git tag v0.1.0 && git push origin v0.1.0` → `.github/workflows/release.yml` builds 4 triples + WASM → GitHub Release with `rsc-ls-<triple>` assets for auto-download
- Zed Marketplace: PR to [`zed-industries/extensions`](https://github.com/zed-industries/extensions) with `extensions.toml` + `pnpm sort-extensions`

`extension.toml` is marketplace-ready (`id`, `homepage`, `repository`, `grammars.rsc` rev pinned).

---

## 📚 Reference

- RouterOS CLI: https://manual.mikrotik.com/docs/cli-reference/
- Truth source: https://manual.mikrotik.com/llms-full.txt
- Zed extensions: https://zed.dev/docs/extensions/developing-extensions
- Grammar repo: https://github.com/balakar94/tree-sitter-rsc

---

## 📄 License

Apache-2.0 — see [LICENSE](LICENSE).

<p align="center">
  Built with ❤️ for MikroTik + Zed
</p>
