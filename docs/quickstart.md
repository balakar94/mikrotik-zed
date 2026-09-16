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

If the server never starts under GUI Zed but works from terminal Zed,
it is the PATH gap — see [GUI Zed ignores PATH](troubleshooting.md#gui-zed-ignores-path).
Binary resolution order and the trust model are covered in
[offline fallback](troubleshooting.md#offline-fallback); caps live in
[lsp-config.md](lsp-config.md).

## 2 · First `.rsc` file

```bash
cat > demo.rsc <<'RSC'
/ip address add address=10.0.0.1/24 interface=ether1
/ip firewall filter add chain=input action=accept
RSC
```

Open `demo.rsc` in Zed.

## 3 · The 3-try loop (one per core feature)

1. **Completion:** type `/ip ` and pause — sub-menus under `/ip` appear
   (see [tiers](language-features.md#completion-tiers)).
2. **Hover:** rest the cursor on `/ip address` — a card with type and arguments.
3. **Diagnostics:** delete `address=` from line 1 — a Warning fires because
   `add` requires it (see [severity ladder](language-features.md#diagnostics)).

## Next

- [Language features](language-features.md) — what each feature covers.
- [Troubleshooting](troubleshooting.md) — logs via `zed: open log`,
  `RSC_LS_LOG=debug` (see [lsp-config.md](lsp-config.md#logging)).
