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

**Documentation hub:** [docs/index.md](docs/index.md) ·
[Quickstart](docs/quickstart.md) · [Configuration](docs/configuration.md) ·
[Recipes](docs/recipes.md) · [Troubleshooting](docs/troubleshooting.md) ·
[Changelog](CHANGELOG.md) · [Roadmap](ROADMAP.md)

## Install

**Not in the Zed extension registry yet (as of 2026-09-19).** Install as a
dev extension today; when the registry entry appears, it will be
Zed → `zed: extensions` → search **MikroTik** → Install.

```bash
make grammar-clone     # fetch grammars/rsc at the pinned rev (see extension.toml)
make install           # full bootstrap; SKIP_SYSTEM=1 skips distro packages
```

Then Zed → Command Palette → *Install Dev Extension* → select this directory.
Opening a `.rsc` file resolves `rsc-ls` from the verified cache or downloads
it from GitHub Releases (checksum-verified before execution). PATH binaries
are **not** used unless you opt in with `RSC_LS_ALLOW_PATH=1` — see
[Configuration](docs/configuration.md#path-trust-model).

Prerequisites: [docs/index.md#prerequisites](docs/index.md#prerequisites)
(editor side, plus RouterOS REST prerequisites for Live/Deploy).

## Features

| Area | What you get |
| ---- | ------------ |
| **Highlighting** | Full RouterOS syntax via a dedicated tree-sitter grammar |
| **Completion** | Menus, verbs, properties, values, script commands (`:`) + snippets and docs |
| **Live data** | Opt-in real-time values from your router (see [Live](docs/live-enrichment.md)) |
| **Hover** | Reference docs for menus, properties, verbs — with a dataset source line |
| **Diagnostics** | Semantic + syntax validation as you type |
| **Outline** | Menu/variable symbols, folding |
| **Signature** | Required-first parameter hints for menu verbs |
| **Navigation** | Go-to-definition / references / rename for `:local` / `:global` vars |
| **Quick fixes** | "Did you mean …?" for typos (edit distance) |
| **Deploy** | Push a validated script over REST or SSH, with verification |
| **Sync** | Command database regenerated from MikroTik's CLI reference |
| **Grammar** | Own repo, pinned by revision in `extension.toml` |

Coverage: the command database models the complete RouterOS CLI snapshot —
see the header of `data/commands.toml` for version, menu count, timestamp,
and source hash. Details: [docs/index.md](docs/index.md).

## Quick start

```bash
cat > demo.rsc <<'RSC'
/ip address add address=10.0.0.1/24 interface=ether1
/ip firewall filter add chain=input action=accept
RSC
```

Open `demo.rsc` in Zed and try the 3-step loop: **completion** (type `/ip `,
pause) → **hover** (rest on `/ip address`) → **diagnostics** (delete
`address=`, watch the Warning). Full walkthrough:
[docs/quickstart.md](docs/quickstart.md).

## Live device data (opt-in)

Disabled by default. Enable it from Zed settings so the server process
receives the variables (terminal exports do not reach a GUI-launched Zed):

```json
{
  "lsp": {
    "rsc-ls": {
      "binary": {
        "env": {
          "RSC_LS_LIVE": "1",
          "MIKROTIK_HOST": "192.168.88.1",
          "MIKROTIK_USER": "admin"
        }
      }
    }
  }
}
```

Put `MIKROTIK_PASS` in your shell profile (captured at Zed startup), never in
a committed settings file. LAN targets also need
`RSC_LS_LIVE_ALLOW_LOOPBACK=1`. Restart Zed, then verify with
`scripts/mikrotik-live-check.py` and the log lines listed in
[docs/live-enrichment.md](docs/live-enrichment.md#verify-it-is-working).

## Deploy

Push the open script to a real router over REST or SSH. Dry-run first (it
still needs `MIKROTIK_HOST`, but no password and no connection):

```bash
MIKROTIK_HOST=192.168.88.1 \
  python scripts/mikrotik-deploy.py demo.rsc --dry-run
```

REST execution is verified with a completion sentinel; `/import` output is
scanned for failure markers. Details, flags, exit codes, and the 6 Zed tasks:
[docs/device-deploy.md](docs/device-deploy.md).

## Upgrading from 0.6.x — behavior changes

- **PATH binaries are now opt-in.** Set `RSC_LS_ALLOW_PATH=1` to run a local
  build from PATH; otherwise the verified cache/download path is used.
- **LAN live targets need `RSC_LS_LIVE_ALLOW_LOOPBACK=1`** (RFC 1918/ULA and
  loopback are denied by default).
- **Deploy verifies REST execution** with the `RSC_DEPLOY_OK` sentinel; a 2xx
  without it is reported as unverified (exit 5). New flags: `--backup`,
  `--keep-file`, `--identity`, `--no-verify-execute`.
- **The `.sha256` companion is parsed strictly** and must name the exact
  asset; it is corruption detection, not provenance.

Full list: [CHANGELOG.md](CHANGELOG.md).

## Development

Everything runs through `make` — run `make help` for the canonical list
(never duplicated here). Daily loop: `make check` (fast gate),
`make validate` (offline gate). Logs:
`RSC_LS_LOG=debug zed --foreground`. QA/CI details:
[docs/qa-ci-release.md](docs/qa-ci-release.md).

## Release

Two tracks: **GitHub Release (automated)** — `make bump VERSION=x.y.z`,
then an **annotated** tag (`git tag -a vX.Y.Z -m "vX.Y.Z" && git push`),
which fires `release.yml`; **Marketplace (human-reviewed)** — PR to
`zed-industries/extensions`; checklist:
[docs/publishing-runbook.md](docs/publishing-runbook.md).

## Docs map

| Page | For |
| ---- | --- |
| [docs/index.md](docs/index.md) | Entry point, prerequisites, component map |
| [docs/quickstart.md](docs/quickstart.md) | Install → first completion in one pass |
| [docs/language-features.md](docs/language-features.md) | Completion, hover, diagnostics |
| [docs/configuration.md](docs/configuration.md) | Env vars, settings, PATH trust |
| [docs/live-enrichment.md](docs/live-enrichment.md) | Opt-in device data |
| [docs/device-deploy.md](docs/device-deploy.md) | Deploy, verification, tasks, exit codes |
| [docs/recipes.md](docs/recipes.md) | Backups, IP, firewall, DHCP recipes |
| [docs/glossary.md](docs/glossary.md) | Terms (path/menu, verb, property, value) |
| [docs/troubleshooting.md](docs/troubleshooting.md) | Symptom → fix matrix, per OS |
| [docs/lsp-config.md](docs/lsp-config.md) | Caps & limits |
| [docs/grammar.md](docs/grammar.md) · [docs/data-pipeline.md](docs/data-pipeline.md) | Grammar, command data |
| [docs/qa-ci-release.md](docs/qa-ci-release.md) · [docs/publishing-runbook.md](docs/publishing-runbook.md) | CI, release, registry |

## Reference

- RouterOS CLI: <https://manual.mikrotik.com/docs/cli-reference/>
- Truth source: <https://manual.mikrotik.com/llms-full.txt>
- Grammar: <https://github.com/balakar94/tree-sitter-rsc>

## License

Apache-2.0 — see [LICENSE](LICENSE).

<p align="center">
  Built with ❤️ for MikroTik + Zed
</p>
