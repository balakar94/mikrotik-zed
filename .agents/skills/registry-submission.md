# Skill: Registry Submission & Release Coherence

## When to Use

Trigger this skill when the task involves any of: `zed-industries/extensions` PRs,
`extensions.toml`, submodule pins, `make bump`, `v*` tags, GitHub Releases,
`package` CI failures, registry review feedback, `sort-extensions`, rebasing the
fork branch, or deciding what may change on `main` while a registry PR is open.

For manifest shape see `zed-extension-dev`; for version-bump mechanics see
`qa-ci-release`; for day-to-day gates see `development-workflow`.

## The One Rule

The registry PR pins an **immutable snapshot**, never a moving branch. These four
must agree at all times, otherwise review stalls or installs break silently:

1. `extensions.toml` (registry fork) `version`
2. `extension.toml` at the pinned submodule commit
3. The `vX.Y.Z` tag pointing at that exact commit
4. The GitHub Release for that tag, with all platform binaries + WASM + SHA-256
   companions published (installs of the new version fail until they exist)

`main` moving ahead never invalidates an open PR. Rewriting history under a pin,
deleting its tag/Release, or pinning a non-tag commit whose binaries were built
elsewhere does.

Runtime note: since 0.7.0 the shim resolves the release tagged with its own
version first, so publishing a newer stable no longer changes the binary an
already-pinned extension downloads. Shims shipped up to 0.6.1 still prefer the
latest stable release and cannot be patched retroactively — with an older pin
live, publish newer stables only once you are ready to move the pin.

## Ordering: Fix Before Bump, Tag After Green

Canonical commit order on `main` per release:

1. Fix commits (code, tasks, docs).
2. `docs: changelog` entry covering everything since the previous tag.
3. `chore: bump version to X.Y.Z` via `make bump` — HEAD must be the bump.
4. Wait for CI green on the bump commit.
5. Create the tag **annotated** (`git tag -a vX.Y.Z -m "vX.Y.Z"`; signed `-s`
   for store submissions), push it, wait for `release.yml` to publish assets.
6. Only then move the registry pin.

Never tag before CI is green. Never push a lightweight tag: `release.yml`
fails them closed (signatures themselves are enforced by push rules).

## What May Change While a PR Is Open

- ✅ New commits on `main` — invisible to the pinned PR.
- ✅ Rebase of the **fork branch** onto upstream `main` (mechanical; see below).
- ❌ History rewrite at or below the pinned commit.
- ❌ Deleting or moving the pinned tag or its Release.
- ❌ Changing `version` in any manifest without a real bump.
- ❌ Pinning a branch tip instead of the tag — same version string, different
  bytes than the Release assets (silent skew: identical version, two states).

## Registry Rebase Flow (fork clone, never inside this repo)

Work in a scratch clone (e.g. the pre-approved temp dir), not in this checkout:

```bash
git clone https://github.com/<you>/extensions.git extensions-rebase
cd extensions-rebase
git remote add upstream https://github.com/zed-industries/extensions.git
git checkout <pr-branch>
git fetch upstream main
git rebase upstream/main   # conflicts land in extensions.toml / .gitmodules:
                           # keep both blocks, yours in alphabetical position
```

Then move the pin (full 40-char SHAs only — never hand-extend an abbreviated
SHA from push output; always `git rev-parse`):

```bash
git submodule update --init extensions/mikrotik-rsc
git -C extensions/mikrotik-rsc fetch origin tag vX.Y.Z
git -C extensions/mikrotik-rsc checkout <full-sha-of-tag>
# extensions.toml: version = "X.Y.Z" (must equal extension.toml at that SHA)
node src/sort-extensions.js   # pnpm equivalent; needs node_modules installed
git add extensions.toml extensions/mikrotik-rsc
git commit -m "Update mikrotik-rsc to X.Y.Z"
git push --force-with-lease origin <pr-branch>
```

Verify: `git diff` shows exactly version + gitlink; `git -C extensions/mikrotik-rsc log --oneline -1`
and `extension.toml` version at the pinned SHA both read `X.Y.Z`.

## Conflict Taxonomy (all previously hit, all mechanical)

| Symptom | Cause | Fix |
|---|---|---|
| `out-of-date with the base branch` | Upstream `main` advanced (hundreds of extensions merge daily); your code is intact | Rebase, no code changes |
| `CONFLICT in extensions.toml` | Neighbor entries added in your alphabetical zone | Keep both blocks, sorted position |
| `CONFLICT in .gitmodules` | Same, plus sort-commit replays | Same; drop the stale trailing-block hunk |
| `unknown variant on_error` in `package` | `tasks.json` `reveal`/`hide` outside Zed's strict enums (`always`/`no_focus`/`never`, `never`/`always`/`on_success`) | Use valid variants; `test_tasks_reveal_hide_schema` guards |
| `requires rustc X.Y` / unknown toolchain | `rust-version` raised above the registry builder | Keep MSRV at the registry toolchain (`rust-toolchain.toml`); `rust-version` is a floor |
| Killed `rsc-ls` on macOS | Kernel `SIGKILL` (Code Signature Invalid) on linker-signed + provenance binaries | `codesign -s -` re-sign; `install-lsp` does it on Darwin |

## PR Hygiene (registry policy)

- Exactly one extension per PR; at most 3 open PRs; reply within 3 weeks or it closes.
- The `out-of-date` notice is informational when merges are clean — rebase on demand, not daily.
- After each push, post a short summary comment (rebase done, pin → tag, version match, checks re-running). Stale silence is what gets PRs closed, not conflicts.
- Batch extension changes into versioned releases with tags; update the pin once per release, never drip-feed.

## Reference

- Runbook: `docs/publishing-runbook.md` (authoritative checklist).
- Registry: <https://github.com/zed-industries/extensions>.
- Zed task schema: `crates/task/src/task_template.rs` in `zed-industries/zed` (`RevealStrategy`, `HideStrategy` — strict, no `on_error` variant exists).
