<h1 align="center">MikroTik RouterOS Script — Zed Extension</h1>

<p align="center">
  Complete Zed integration for MikroTik RouterOS Script<br>
  <em>Tree-sitter highlighting · LSP completion & hover · Diagnostics · Deploy</em>
</p>

<p align="center">
  <a href="https://github.com/balakar94/mikrotik-zed/releases"><img src="https://img.shields.io/github/v/release/balakar94/mikrotik-zed?label=release&color=blue" alt="release"></a>
  <a href="https://github.com/balakar94/mikrotik-zed/actions/workflows/ci.yml"><img src="https://github.com/balakar94/mikrotik-zed/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/balakar94/mikrotik-zed/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-green" alt="license"></a>
  <a href="https://manual.mikrotik.com/docs/cli-reference/"><img src="https://img.shields.io/badge/RouterOS-v7.20%2B-red" alt="RouterOS"></a>
  <a href="https://zed.dev"><img src="https://img.shields.io/badge/Zed-extension-black" alt="Zed"></a>
  <a href="https://github.com/balakar94/tree-sitter-rsc"><img src="https://img.shields.io/badge/tree--sitter-rsc-orange" alt="grammar"></a>
</p>

---

**Contents:** [Features](#-features) · [Install](#-install) · [Quick start](#-quick-start) · [Deploy](#-deploy) · [Live](#-live-device-enrichment-opt-in) · [Language server](#-language-server) · [Grammar](#-grammar) · [Sync](#-sync--extraction) · [Development](#️-development) · [Release](#-release) · [Reference](#-reference) · [License](#-license)

Start here: [docs/index.md](docs/index.md) · [docs/quickstart.md](docs/quickstart.md) · Full history: [CHANGELOG.md](CHANGELOG.md) · [ROADMAP.md](ROADMAP.md)

---

## ✨ Features

| Area | What you get |
| ---- | ------------ |
| **Highlighting** | Full RouterOS syntax via a dedicated tree-sitter grammar |
| **Completion** | Menus, verbs, properties, values + snippets and docs |
| **Live data** | Opt-in real-time values from your router (see Live) |
| **Hover** | Reference docs for menus, properties, verbs |
| **Diagnostics** | Semantic + syntax validation as you type |
| **Outline** | Menu/variable symbols, folding |
| **Signature** | Required-first parameter hints for menu verbs |
| **Navigation** | Go-to-definition / references for `:local` / `:global` vars |
| **Quick fixes** | "Did you mean …?" for typos (edit distance) |
| **Deploy** | Push a validated script to a router over REST or SSH |
| **Sync** | Command database regenerated from MikroTik's CLI reference |
| **Grammar** | Own repo, pinned by revision in `extension.toml` |

**Coverage:** the command database models the complete RouterOS CLI snapshot (see the header of `data/commands.toml` for version, menu count, timestamp, and source hash) — directories and executable commands across the whole tree, compatible with 7.20+ and broadly usable on 7.0+ for common menus. Details: [docs/index.md](docs/index.md).

<details>
<summary>Example <code>.rsc</code> — hover, completion, diagnostics</summary>

```rsc
/ip address add address=10.0.0.1/24 interface=ether1
/ip firewall filter add chain=input action=accept comment="allow"

/ip address add interface=ether1   # diagnostics: missing required `address` → Warning

/tool fetch url="https://example.com/long/url" \
    mode=https
:if ($var > 10) do={ :put "ok" } else={ :error "fail" }
```

</details>

---

## 📦 Install

**Registry (once published):** Zed → `zed: extensions` → search **MikroTik** → **Install**.

**Dev install:**

```bash
cargo build -p rsc-ls --release && export PATH="$PWD/target/release:$PATH"
# Zed → Command Palette → Install Dev Extension → select this directory
# Logs: zed: open log
```

Full bootstrap (toolchains, grammar, LSP): see [docs/quickstart.md](docs/quickstart.md).

**Binary auto-download:** opening a `.rsc` file resolves `rsc-ls` via PATH → cache → GitHub Releases (SHA-256 verified before execution). Assets are named by Rust target triple:

| Triple | Platform |
| ------ | -------- |
| `aarch64-apple-darwin` | macOS Apple Silicon |
| `x86_64-apple-darwin` | macOS Intel |
| `aarch64-unknown-linux-gnu` | Linux ARM64 |
| `x86_64-unknown-linux-gnu` | Linux x64 |
| `x86_64-pc-windows-msvc` | Windows x64 |
| `aarch64-pc-windows-msvc` | Windows ARM64 |

---

## 🚀 Quick start

```bash
cat > demo.rsc <<'RSC'
/ip address add address=10.0.0.1/24 interface=ether1
/ip firewall filter add chain=input action=accept
RSC
```

Open `demo.rsc` in Zed and try the 3-step loop: **completion** (type `/ip `, pause) → **hover** (rest on `/ip address`) → **diagnostics** (delete `address=`, watch the Warning). Full walkthrough: [docs/quickstart.md](docs/quickstart.md).

---

## 🛫 Deploy

Push the open script to a real router over REST or SSH (REST default, SSH via SFTP + `/import`); output is scanned for RouterOS failure markers since the device often answers 200 / exit 0 on failed imports. Configure via `MIKROTIK_HOST` / `MIKROTIK_USER` / `MIKROTIK_PASS` (`scripts/mikrotik-deploy.py --help`). Always dry-run first:

```bash
python scripts/mikrotik-deploy.py demo.rsc --dry-run
```

Zed tasks (`languages/rsc/tasks.json` → copy to `.zed/tasks.json`): REST / SSH / dry-run / validate. Details: [docs/device-deploy.md](docs/device-deploy.md).

---

## ⚡ Live Device Enrichment (opt-in)

<details>
<summary><b>Expand Live configuration</b> (disabled by default)</summary>
<br>

`rsc-ls` enriches completion with live router data (interfaces, addresses, firewall lists/chains, pools). Live items sort first with the `0!live_...` key.

```bash
export RSC_LS_LIVE=1                  # or MIKROTIK_LIVE=1
export MIKROTIK_HOST="192.168.88.1"
export MIKROTIK_USER="admin"          # default: admin
export MIKROTIK_PASS="secret"         # env/keychain only — never logged
export MIKROTIK_PORT=443              # default 443
export MIKROTIK_TIMEOUT=5             # seconds, clamped 1..30
export MIKROTIK_SSL=0                 # 0 = skip TLS verify (self-signed)
export MIKROTIK_HTTP=1                # 1 = plain HTTP (port 80 routers)
export RSC_LS_LEGACY_HTTP_SHIM=1      # opt-in: allow port-80 + SSL=0 → http fallback (OFF by default)
export RSC_LS_ALLOW_SETTINGS_TRANSPORT=1  # opt-in: honor host/user/TLS keys from workspace settings
```

**Settings gate:** transport keys (`host`/`user`/TLS) in Zed workspace settings are **ignored** unless `RSC_LS_ALLOW_SETTINGS_TRANSPORT=1`. A password in workspace settings is **always ignored with a warning** — env/keychain is the sole password source. Commit `.zed/settings.json` only if secret-free.

**Health check:** `python scripts/mikrotik-live-check.py --dry-run`, then without flags for a real `GET /rest/interface` (`Live OK: N interfaces`, else `Live FAIL` / exit 4). Live cache is in-memory only (60s TTL), completion never blocks (2s coalesce, 15s negative cache, 512 KiB / 500-item caps, max 4 hosts / 8 custom resources, SSRF deny `169.254.169.254`). Full reference: [docs/live-enrichment.md](docs/live-enrichment.md).

</details>

---

## 🧠 Language Server

`rsc-ls` is a pure-Rust LSP over stdio with the command table compiled in (`include_str!()`): context-aware completion (triggers `/`, space, `=`, `:`), hover from upstream docs, two-layer diagnostics (semantic signatures + syntax incl. backslash continuations), symbols/folding, signature help, variable navigation, typo quick-fixes — behind message/document size caps and strict `file://` validation. Deep dive: `lsp/src/` and [docs/language-features.md](docs/language-features.md). Caps: [docs/lsp-config.md](docs/lsp-config.md).

---

## 🌳 Grammar

Highlighting comes from [`balakar94/tree-sitter-rsc`](https://github.com/balakar94/tree-sitter-rsc), kept as an untracked working copy in `grammars/rsc/` and pinned by `rev` in `extension.toml` (`make grammar-clone` fetches it). `languages/rsc/*.scm` is canonical for Zed; only `highlights.scm` is mirrored into the grammar repo. Publish via `python scripts/publish_grammar.py` (never hand-edit `rev`). Details: [docs/grammar.md](docs/grammar.md).

---

## 🔄 Sync & Extraction

`scripts/sync_llms.py` fetches `llms-full.txt` → `scripts/extract_commands.py` distills `data/commands.toml` (header carries version, timestamp, source hash; provenance in `data/upstream-docs.toml`). Refresh with `make sync && make extract`; CI gates drift separately (`make sync-check`) and a weekly workflow files the `upstream-docs` issue. Details: [docs/data-pipeline.md](docs/data-pipeline.md).

---

## 🛠️ Development

Everything runs through `make` — run `make help` for the canonical list (never duplicated here). Daily loop: `make check` (fast gate), `make validate` (offline gate: manifest, docs, fmt, clippy, tests, extract). Logs: `RSC_LS_LOG=debug zed --foreground`. QA/CI details: [docs/qa-ci-release.md](docs/qa-ci-release.md).

---

## 📤 Release

Two tracks: **1 · GitHub Release (automated)** — `make bump VERSION=x.y.z`, then `git tag vX.Y.Z && git push` fires `release.yml` (six triples + WASM + SHA-256). **2 · Marketplace (human-reviewed)** — PR to `zed-industries/extensions`; checklist: [docs/publishing-runbook.md](docs/publishing-runbook.md), local check `make check-manifest`.

---

## 📚 Reference

- RouterOS CLI: <https://manual.mikrotik.com/docs/cli-reference/>
- Truth source: <https://manual.mikrotik.com/llms-full.txt>
- Docs: [index](docs/index.md) · [quickstart](docs/quickstart.md) · [live](docs/live-enrichment.md) · [deploy](docs/device-deploy.md) · [qa/release](docs/qa-ci-release.md) · [publishing](docs/publishing-runbook.md)
- Grammar: <https://github.com/balakar94/tree-sitter-rsc>
- [CHANGELOG.md](CHANGELOG.md) · [ROADMAP.md](ROADMAP.md)

---

## 📄 License

Apache-2.0 — see [LICENSE](LICENSE).

<p align="center">
  Built with ❤️ for MikroTik + Zed
</p>
