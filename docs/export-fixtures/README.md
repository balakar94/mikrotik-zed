# Export fixtures (sanitized `/export` samples)

Small anonymized RouterOS `/export`-style samples used to cross-check
`data/commands.toml`: every `key=` property in these files must exist for its
menu in the generated table (see
`tests/test_commands_coverage.py::TestExportFixtures`).

- **Sanitized and anonymized.** Addresses come from the TEST-NET ranges
  (`192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`); interface names are
  generic (`ether1`, `ether2`); comments are placeholders. **Never commit
  real IPs, MACs, usernames, passwords, keys, certificates, or device
  identities here.**
- **Source.** No device was available when these were created: each sample
  was hand-built from the property names documented in `llms-full.txt` and
  the upstream CLI reference (manual.mikrotik.com), then verified against
  `data/commands.toml`. They illustrate shape, not a real device export.
- **Coverage.** At least one fixture per top `REQUIRED_MENUS` area; current
  set: `/ip/address`, `/ip/route`, `/system/scheduler`.
- **Format.** One menu section per file: a `/path` header line followed by
  `add` rows with `key=value` pairs (values quoted when they contain
  spaces). The test parses them with `shlex` and asserts every key exists
  under its menu's `flags`/`arguments`/`read_only` in `commands.toml`
  (generic add-item names such as `comment`/`disabled` count as present).
- **Adding a fixture.** Copy the shape of the existing files, keep values
  fake, and run
  `pytest tests/test_commands_coverage.py -k export_fixture` before opening
  a PR. If the new property legitimately does not exist upstream, add a
  curated entry to `data/overrides.toml` (verifiable on-device) instead of
  editing `data/commands.toml`, which is generated.
