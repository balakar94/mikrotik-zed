# Quickstart

Install the extension, open a `.rsc` file, verify the three core features.
For the full install matrix see `README.md` Install; for commands see `make help`.

## 1 · Install

**From Zed Extensions (once published):** Zed → `zed: extensions` →
search **MikroTik** → Install.

**Dev install (local):**

```bash
make grammar-clone     # fetch grammars/rsc at the pinned rev (see extension.toml)
make install           # full bootstrap; SKIP_SYSTEM=1 skips distro packages
```

Then Zed → Command Palette → *Install Dev Extension* → select this directory.

**GUI PATH note (macOS):** GUI Zed (Dock) does not inherit shell PATH.
`make install-lsp` copies `rsc-ls` to a GUI-visible location as well —
or copy the binary yourself. If the server never starts under GUI Zed
but works from terminal Zed, this is the cause.
See [troubleshooting.md](troubleshooting.md#ls-not-starting).

## 2 · Binary resolution

No manual build is required. On opening a `.rsc` file the shim resolves
`rsc-ls` in order, first success wins:

1. **PATH** — your own `rsc-ls` (dev override; bypasses checksum gate,
   warning is logged — keep only trusted builds on PATH).
2. **Cache** — previously downloaded copy, reused as-is.
3. **GitHub Releases** — matching platform asset, SHA-256 verified
   *before* execution. Any failure aborts with manual instructions;
   an unverified binary is never executed.

## 3 · First `.rsc` file

```bash
cat > demo.rsc <<'RSC'
/ip address add address=10.0.0.1/24 interface=ether1
/ip firewall filter add chain=input action=accept
RSC
```

Open `demo.rsc` in Zed.

## 4 · The 3-try loop (one per core feature)

1. **Completion:** type `/ip ` and pause — sub-menus under `/ip` appear
   (see [tiers](language-features.md#completion-tiers)).
2. **Hover:** rest the cursor on `/ip address` — a card with type and arguments.
3. **Diagnostics:** delete `address=` from line 1 — a Warning fires because
   `add` requires it (see [severity ladder](language-features.md#diagnostics)).

## Next

- [Language features](language-features.md) — what each feature covers.
- [Troubleshooting](troubleshooting.md) — logs via `zed: open log`,
  `RSC_LS_LOG=debug` (see [lsp-config.md](lsp-config.md#logging)).
