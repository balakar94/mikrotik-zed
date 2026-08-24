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

**Contents:** [Features](#-features) · [Install](#-install) · [Quick start](#-quick-start) · [Dependencies](#-dependencies) · [Deploy](#-deploy) · [Language Server](#-language-server) · [Grammar](#-grammar) · [Sync & Extraction](#-sync--extraction) · [Development](#️-development) · [Release](#-release) · [Reference](#-reference) · [License](#-license)

---

## ✨ Features

| Area             | What you get                                                                                              |
| ---------------- | --------------------------------------------------------------------------------------------------------- |
| **Highlighting** | Full RouterOS syntax highlighting powered by a dedicated tree-sitter grammar                              |
| **Completion**   | Context-aware suggestions for menus, verbs, properties and values, with snippets and inline documentation |
| **Hover**        | Reference documentation for menus, properties and verbs, shown right where you type                       |
| **Diagnostics**  | Live semantic and syntax validation of your script while you write it                                     |
| **Outlining**    | Document outline of menus and script variables, plus code folding                                         |
| **Signature**    | Parameter hints when calling menu verbs                                                                   |
| **Navigation**   | Go-to-definition and find-references across script variables                                              |
| **Quick fixes**  | "Did you mean …?" corrections for typos in properties, menus and values                                   |
| **Deploy**       | Send a validated script to a real router over REST or SSH without leaving the editor                      |
| **Sync**         | Keeps the built-in command database aligned with MikroTik's published CLI reference                       |
| **Grammar**      | A dedicated tree-sitter grammar, developed in its own repository and pinned by revision                   |

**Coverage:** the command database models the complete RouterOS v7 CLI — **1038 menus**, directories and executable commands alike, spanning every context of the hierarchy from interfaces and networking to queues, users and system tools. It is extracted automatically from MikroTik's official machine-readable documentation, so completion, hover and diagnostics work anywhere in the tree rather than on a hand-picked subset.

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

Using the extension requires no manual builds. When you open a `.rsc` file, the extension locates its language server by trying three sources in order, stopping at the first success:

1. **Your PATH** — an `rsc-ls` you installed yourself takes precedence (the development override; Windows probes `rsc-ls.exe` too).
2. **The cache** — the copy downloaded by a previous session, reused as-is.
3. **GitHub Releases** — otherwise it downloads the build that matches your platform from the table below, verifies it against the published SHA-256 companion _before_ making it executable or running it, and surfaces progress through Zed's installation-status UI. Any failure — missing asset, checksum mismatch — aborts cleanly and shows manual instructions: an unverified binary is never executed.

| Triple                      | Platform            |
| --------------------------- | ------------------- |
| `aarch64-apple-darwin`      | macOS Apple Silicon |
| `x86_64-apple-darwin`       | macOS Intel         |
| `aarch64-unknown-linux-gnu` | Linux ARM64         |
| `x86_64-unknown-linux-gnu`  | Linux x64           |
| `x86_64-pc-windows-msvc`    | Windows x64         |
| `aarch64-pc-windows-msvc`   | Windows ARM64       |

> **Windows detail:** the downloaded binary is cached and spawned with an `.exe` suffix, because Windows refuses to execute images whose file name lacks one. Release assets themselves stay extension-less byte blobs, so a single naming scheme serves every platform.
>
> **Fresh-release 404s:** GitHub's CDN can take a minute or two to propagate new release assets; the API path works immediately, and a PATH-installed binary always bypasses the download entirely.

---

## 🚀 Quick start

Create a script in a folder where you actually keep your RouterOS work — any `.rsc` file will do, they are plain text:

```bash
cat > demo.rsc <<'RSC'
/ip address add address=10.0.0.1/24 interface=ether1
/ip firewall filter add chain=input action=accept
RSC
```

Open `demo.rsc` in Zed and try three things, one per core feature:

- **Completion:** type `/ip ` and pause — the popup lists the sub-menus that exist under `/ip` according to the command database.
- **Hover:** rest the cursor on `/ip address` — a card explains what the menu is for and what its commands accept.
- **Diagnostics:** remove `address=` from the first line — the editor flags that `add` under `/ip address` cannot run without it, because validation knows the real command signature rather than guessing from syntax alone.

---

## 🔧 Dependencies

Nothing exotic is required, and `make install` sets up all of it for you (see Bootstrap below). This is the complete list, and why each piece exists:

| Dependency                 | Required          | Why                                                                                                                                                                                                                       |
| -------------------------- | ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Rust, via rustup           | ✅ required       | Builds the language server and the extension glue. The repo pins the compiler version _and_ the WebAssembly target in `rust-toolchain.toml`, so rustup installs both automatically on first use — nothing to add by hand. |
| Python 3.11+               | ✅ required       | Runs the extraction, sync and deploy tooling (it relies on the standard-library `tomllib`).                                                                                                                               |
| pytest, requests, paramiko | ✅ auto-installed | The Python test suite and the deploy transports. They live inside a project-local `.venv`, never the system interpreter — which sidesteps PEP 668 on Fedora, Debian and Arch.                                             |
| C compiler + linker        | ✅ required       | Compiles the native language server (Xcode CLT, `build-essential`, or `base-devel` depending on platform).                                                                                                                |
| git                        | ✅ required       | Clones the grammar working copy at the revision pinned in `extension.toml`.                                                                                                                                               |
| curl                       | ✅ required       | Downloads toolchains and release binaries during setup.                                                                                                                                                                   |
| cargo-audit                | ⚪ optional       | Scans dependencies against the RustSec advisory database. It runs weekly in CI; `make audit` does the same locally.                                                                                                       |
| wasm-tools                 | ⚪ optional       | Inspects the compiled WebAssembly component when hacking on the extension glue (rustc already emits a valid component).                                                                                                   |

### Bootstrap

`make install` performs the entire setup and is idempotent — re-running it never breaks anything. It detects your operating system and package manager, installs the system packages, provisions rustup with a minimal profile if it is missing (the pinned toolchain and WASI target then arrive on their own through `rust-toolchain.toml`), creates the `.venv` with the Python dependencies, installs a local tree-sitter CLI, and finally builds the language server and puts it on your PATH — including the GUI-visible location on macOS, so the Zed app finds it too.

- Containers and CI usually want `SKIP_SYSTEM=1 make install` to skip distro packages.
- Every stage is also a standalone target: `make install-deps` (system packages), `make install-tools` (toolchains and `.venv`), `make install-lsp` (build + install the server).

Supported platforms:

- **macOS** — Homebrew, plus Xcode Command Line Tools
- **Fedora / RHEL** — dnf or yum
- **Arch** — pacman
- **Debian / Ubuntu** — apt

Any other system works too: skip the distro step with `SKIP_SYSTEM=1` and provide the equivalent packages yourself.

<details>
<summary>Per-platform notes</summary>

macOS (Homebrew):

```bash
xcode-select --install
brew install python node
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
```

Fedora/RHEL:

```bash
sudo dnf install -y gcc gcc-c++ make curl git openssl-devel pkgconf-pkg-config python3 python3-pip nodejs npm
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
```

Debian/Ubuntu:

```bash
sudo apt-get update
sudo apt-get install -y build-essential curl git pkg-config libssl-dev python3 python3-venv python3-pip nodejs npm
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
```

Arch:

```bash
sudo pacman -Sy --needed --noconfirm base-devel curl git python python-pip nodejs npm
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
```

Then, on any platform:

```bash
make install-tools   # .venv + tree-sitter-cli; toolchain and WASI target come pinned via rust-toolchain.toml
make grammar-clone   # fetch the grammars/rsc working copy
```

</details>

---

## 🛫 Deploy

Local validation only proves the script is well-formed; sooner or later you want it running on a router. Doing that by hand means copying text into a terminal session and hoping the paste survived quoting intact. The deploy tooling closes that gap: it pushes the file to the device and imports it, either from a Zed task or from the companion script, with a dry-run mode that exercises the whole path without ever touching a device.

Two transports are available, selected per task:

- **REST** — talks to the RouterOS 7 REST API, posting the script for execution (with a file-upload + `/import` fallback for longer scripts).
- **SSH** — uploads via SFTP and runs `/import` over an interactive session.

Both report more than a transport-level success code: RouterOS routinely answers HTTP 200 or exits SSH with status 0 even when an import failed, so the output is scanned for high-confidence failure markers (`syntax error`, `bad command name`, `failure:` …) and surfaces them as real failures.

The connection is configured through environment variables (`MIKROTIK_HOST`, `MIKROTIK_USER`, `MIKROTIK_PASS`, plus optional overrides for port, method, TLS verification and timeout — see `python scripts/mikrotik-deploy.py --help`). One safe first run needs nothing else:

```bash
python scripts/mikrotik-deploy.py demo.rsc --dry-run
```

Zed tasks live per-worktree: copy `languages/rsc/tasks.json` to `.zed/tasks.json` and you get **REST / SSH / Dry-run / Validate** entries that operate on `$ZED_FILE`.

---

## 🧠 Language Server

The heart of the extension is **rsc-ls**, a self-contained language server written in pure Rust — no Node runtime, no external processes: one binary speaking the Language Server Protocol over stdio. Its entire knowledge of RouterOS, the 1038-menu database, is compiled into the executable itself, which means instant startup and zero data files to lose track of.

That database drives everything contextual the editor experiences:

- **Completions** understand where the cursor is: root menus after a leading `/`, sub-menus within the current context, the verbs a specific menu accepts, and per-verb properties and values — including enumerations, booleans and their documentation. After a `:` it switches to RouterOS scripting words. Triggers are `/`, space, `=` and `:`.
- **Hover** resolves menu paths, properties and verbs against the same database, so the documentation shown is the documentation MikroTik publishes, not paraphrasing.
- **Diagnostics** work in two layers. Semantic rules check each statement against the real command signatures — unknown menus, unknown properties, missing required arguments, duplicated properties, invalid enum values. Syntax rules catch structural breakage such as brace and quote mismatches, correctly handling backslash line continuations by reasoning about logical lines while reporting positions on physical ones. Generous caps on file size keep the editor responsive on huge export scripts.
- **Structure** features derive from the same parse: document symbols for menus and script variables, plus folding ranges.
- **Signature** help shows required-first parameter hints when calling menu verbs.
- **Navigation** links variable declarations (`:local` / `:global`) to their `$name` usages in both directions.
- **Quick fixes** offer "did you mean …?" candidates computed by edit distance when something is mistyped.

The protocol side is hardened deliberately: message-size and document-size caps, a bounded document store, and strict URI validation — so a hostile or corrupt workspace cannot exhaust the server's memory.

---

## 🌳 Grammar

RouterOS has no standard tree-sitter grammar, so this project maintains one — but in its own repository, [`balakar94/tree-sitter-rsc`](https://github.com/balakar94/tree-sitter-rsc), rather than in-tree. Two reasons drive the split: the grammar iterates independently of the extension, and the Zed marketplace rejects packages containing nested git repositories. Instead of vendoring history, this repo keeps an **untracked working copy** at `grammars/rsc/` and pins the exact grammar revision in `extension.toml` — builds stay reproducible while the packaging stays clean. After cloning, `make grammar-clone` fetches the working copy at that revision.

Query files exist twice by design: `languages/rsc/*.scm` here is canonical (it is what Zed loads), while the copy inside the grammar repository exists only so `tree-sitter test` can run against it.

Grammar development is the **only** place Node.js enters this project — it provides `tree-sitter-cli` through npx:

```bash
cd grammars/rsc
npx tree-sitter generate       # grammar.js → src/parser.c
npx tree-sitter test           # run the corpus suite
npx tree-sitter parse FILE     # inspect how a file parses
npx tree-sitter highlight FILE # preview highlighting
```

Publishing a grammar change is scripted: `python scripts/publish_grammar.py` pushes the grammar repository and updates the revision pin in `extension.toml` in one step.

---

## 🔄 Sync & Extraction

RouterOS evolves, and a hardcoded command table would rot silently. MikroTik mitigates this by publishing its CLI reference in machine-readable form (`llms-full.txt`), and this project builds directly on that source of truth.

The pipeline has two steps. `scripts/sync_llms.py` fetches the upstream files and compares them against what the database was built from — with `--check` it writes nothing and exits non-zero when upstream moved, which is exactly how CI notices drift. `scripts/extract_commands.py` then distills the fetched documentation into `data/commands.toml`: currently 1038 menus, regenerated idempotently. Each generation records the RouterOS version, the UTC timestamp and the hash of the source document in its header, so any database snapshot can be traced back to the exact documentation it came from.

Updating the language server's knowledge therefore reduces to: sync, extract, commit — the TOML is embedded into the binary at compile time, and the next build carries the fresh data everywhere.

---

## 🛠️ Development

Everything runs through `make`, so contributor workflows mirror CI one-to-one: `make validate` replays the entire gate locally — formatting, clippy on both targets with warnings denied, all three test suites, upstream-sync check and extraction idempotency. The combined suite is 838 tests: 67 grammar corpus tests, 555 Rust tests (537 unit + 4 CLI + 14 end-to-end) and 216 Python tests.

```bash
make generate      # regenerate parser.c from grammar.js
make test-all      # grammar + Rust + Python suites
make check         # fast compile verification (WASM + LSP targets)
make fmt clippy    # format check + lints (-D warnings)
make audit         # dependency scan against RustSec advisories
make validate      # everything CI runs, in one target
```

**Observability:** the server logs to stderr (stdout belongs to the protocol):

```bash
RSC_LS_LOG=debug zed --foreground      # or: RUST_LOG
# levels: error < warn < info < debug < trace (default info)
# prefixes: [rsc-ls][LEVEL] and [mikrotik-zed]
```

---

## 📤 Release

A release is fully automated behind one action: push a tag. Concretely, bump versions with `make bump VERSION=x.y.z`, then `git tag vX.Y.Z && git push origin vX.Y.Z`. The release workflow cross-compiles the language server for all six platform triples, builds the WebAssembly component, and publishes a GitHub Release containing every binary together with its SHA-256 companion — which is precisely what the extension's auto-download verifies at install time. Distribution through the Zed marketplace is a separate, human-reviewed step: a pull request to [`zed-industries/extensions`](https://github.com/zed-industries/extensions) carrying the extension metadata and the pinned grammar revision. `extension.toml` is kept marketplace-ready at all times.

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
