# Quickstart

Install the extension, open a `.rsc` file, verify the core features. For
device prerequisites see [index.md](index.md#prerequisites); for commands see
`make help`.

## 1 · Install

**Registry:** not in the Zed extension registry yet (as of 2026-09-19) — the
instructions below are the supported path today. When the registry entry
appears, installation is Zed → `zed: extensions` → search **MikroTik** →
Install, with no local toolchain.

**Dev install (current path):**

```bash
make grammar-clone     # fetch grammars/rsc at the pinned rev (see extension.toml)
make install           # full bootstrap; SKIP_SYSTEM=1 skips distro packages
```

Then Zed → Command Palette → *Install Dev Extension* → select this directory.
`make install` builds `rsc-ls`, so the toolchain requirement is Rust plus the
`wasm32-wasip2` target; `make install-tools` sets those up.

On first `.rsc` open, the shim resolves the server:
verified cache → auto-download (needs network) → **not** PATH unless you opt
in. PATH lookup is denied by default since the security model favors verified
binaries; set `RSC_LS_ALLOW_PATH=1` to run a local build from PATH. Per-OS
paths and failure modes:
[troubleshooting.md#path-and-gui-zed](troubleshooting.md#path-and-gui-zed).

## 2 · First `.rsc` file

```bash
cat > demo.rsc <<'RSC'
/ip address add address=10.0.0.1/24 interface=ether1
/ip firewall filter add chain=input action=accept
RSC
```

Open `demo.rsc` in Zed.

## 3 · The 3-try loop (one per core feature)

1. **Completion:** type `/ip ` and pause — sub-menus under `/ip` appear,
   required properties first (see
   [tiers](language-features.md#completion-tiers)). Then type `:` on a new
   line to see script commands like `:local` and `:if`.
2. **Hover:** rest the cursor on `/ip address` — a card with type and
   arguments, followed by the dataset source line.
3. **Diagnostics:** delete `address=` from line 1 — a Warning fires because
   `add` requires it (see [severity ladder](language-features.md#diagnostics)).

## 4 · Next steps

- Configure the server or enable live device data:
  [Configuration](configuration.md).
- Make a real change safely: [Recipes](recipes.md) and
  [Device deploy](device-deploy.md).
- Something broken? [Troubleshooting](troubleshooting.md) — logs via
  `zed: open log`, `RSC_LS_LOG=debug`
  (see [configuration](configuration.md#environment-variables)).
