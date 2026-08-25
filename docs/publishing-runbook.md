# Publishing Runbook — `zed-industries/extensions`

## Purpose

Step-by-step checklist for submitting this extension to the [Zed extension registry](https://github.com/zed-industries/extensions) and for shipping updates to it. The registry review process enforces rules our local tooling cannot check; the audit of the v0.5.0 submission produced the corrections captured here. Work through it top-to-bottom for every registry PR.

## Source requirements

These pages are authoritative — re-check them instead of trusting any summary, including this one:

- <https://zed.dev/docs/extensions/publishing/prerequisites>
- <https://zed.dev/docs/extensions/publishing/license-requirements>
- <https://zed.dev/docs/extensions/publishing/publishing-guide>
- <https://zed.dev/docs/extensions/publishing/updating-and-maintenance>
- <https://zed.dev/docs/extensions/developing-extensions>

Rules enforced by registry maintainers:

- Every PR adds or updates **exactly one** extension.
- Keep at most **three open PRs** at a time.
- Reply to maintainer feedback within **three weeks**, or the PR is closed.

## One-time submission checklist

1. Run the local gates (see below).
2. Manually test the extension in Zed **at the exact submodule commit being submitted**: check out this repo at that commit and use _Install Dev Extension_. Do not skip this because a nearby commit worked.
3. Fork `zed-industries/extensions`.
4. Add the submodule using an HTTPS URL under `extensions/<extension-id>` — for this project: `extensions/mikrotik-rsc`.
5. Verify the checked-out submodule commit is **on a branch, never detached**.
6. Add a top-level `extensions.toml` entry:

   ```toml
   [mikrotik-rsc]
   submodule = "extensions/mikrotik-rsc"
   version = "x.y.z"
   ```

   The `version` here **must equal** the `version` in this repo's `extension.toml` at the pinned submodule commit — a mismatch stalls review.
7. Run `pnpm sort-extensions`.
8. Open the PR covering only this one extension, and watch it under the response-time rule above.

License: Apache-2.0 with `LICENSE` at the extension root is accepted by policy — already satisfied in this repository (verified).

## Update checklist

1. Bump the version: `make bump VERSION=x.y.z`.
2. Tag `vX.Y.Z` and push the tag — this triggers `.github/workflows/release.yml`.
3. Wait for the Release workflow to publish its assets (multi-platform `rsc-ls` binaries, WASM component, SHA-256 companions); installs of the new version fail until they exist.
4. In your `zed-industries/extensions` fork:
   - `git submodule update --remote extensions/mikrotik-rsc`
   - update `version` in `extensions.toml` so it matches the new `extension.toml` version
   - run `pnpm sort-extensions`
5. Open the PR under the same registry rules (one extension, ≤3 open, reply ≤3 weeks).

## Local gates before every submission/update

```bash
make validate               # full pre-commit/pre-PR gate
make check-manifest         # Zed requirements compliance (also part of validate and CI)
```

`check-manifest` (script: `scripts/check_zed_requirements.py`) currently enforces the
manifest half of Zed's requirements: `extension.toml` may contain only keys that Zed's
`ExtensionManifest` schema actually knows. Unknown keys such as a stray `homepage` or `categories`
are silently ignored by Zed — they never error at runtime, they just mask typos and dead config
until a reviewer spots them. Keep the manifest strictly schema-known.

## Compatibility notes

The extension builds against the latest `zed_extension_api` (currently 0.7.x). Per Zed's own statement, extensions built on newer API versions are not loadable by older Zed releases. Before promising any minimum supported Zed version, consult the official compatibility table at <https://github.com/zed-industries/zed/blob/main/crates/extension_api/README.md> — it may lag the newest API rows, so treat it as the floor, not the ceiling of what exists. Whenever the API crate is upgraded in this repo, mention the bump in the release notes.

## Packaging trade-off (accepted)

Registry packaging ships the entire submodule tree to users. This monorepo deliberately keeps development tooling (Makefile, `tests/`, `scripts/`, `docs/`) tracked, so users receive more than the strict minimum a packaged extension needs to function. This deviation is accepted consciously: the user-facing payload remains tiny (text files only), while binaries and WASM stay untracked. Revisit only if maintainers raise it during review.
