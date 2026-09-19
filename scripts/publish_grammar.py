#!/usr/bin/env python3
"""
Publish the grammar working copy (grammars/rsc -> balakar94/tree-sitter-rsc)
and update extension.toml rev. grammars/rsc is an untracked working copy —
nested gitlinks break zed-industries/extensions packaging, so the parent repo
never records a pointer; extension.toml [grammars.rsc].rev is the only pin.

By default uses the repo at https://github.com/balakar94/tree-sitter-rsc.
For local dev, can push to the bare repo at grammar-bare.git.

Usage:
  python scripts/publish_grammar.py --dry-run   # show what would be done
  python scripts/publish_grammar.py --push      # push to remote + update rev
  python scripts/publish_grammar.py --push --remote grammar-bare  # local bare
  python scripts/publish_grammar.py --push --branch release/0.7.0  # non-main target
  python scripts/publish_grammar.py --push --no-commit  # stage allowlist only, print manual commit steps

Steps:
  1) Ensure grammars/rsc is a git repo (init if needed; never in --dry-run)
  2) Fail-closed generation gate: npx tree-sitter generate + git diff --exit-code
     on src/parser.c, src/grammar.json and src/node-types.json
  3) Fail-closed corpus gate: npx tree-sitter test
  4) Commit any changes in grammars/rsc (if dirty)
  5) Push HEAD to the target branch (--branch, default main)
  6) Get new HEAD SHA
  7) Update extension.toml [grammars.rsc].rev
  8) (Optional) bump Cargo.lock handling: run cargo generate-lockfile, keep Cargo.lock committed

--dry-run is strictly side-effect free: no git init, no remote add, no
generate/test, no commit, no push, no extension.toml write.

Version bumps are manual: edit grammars/rsc/Cargo.toml, grammars/rsc/package.json, extension.toml version together.
"""
from __future__ import annotations

import argparse
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
GRAMMAR_DIR = ROOT / "grammars" / "rsc"
EXT_TOML = ROOT / "extension.toml"
BARE_REPO = ROOT / "grammar-bare.git"

# Explicit allowlist of grammar source paths staged for publish. Build
# outputs and caches (target/, build/, node_modules/, *.log) are never
# staged: a blanket add of every change could sweep local artifacts into the
# grammar history. Only these versioned sources are published. The
# extension.toml rev pointer is updated by this script only, never by hand.
GRAMMAR_ALLOWLIST = [
    "grammar.js",
    "queries",
    "src",
    "test",
    "bindings",
    "tree-sitter.json",
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
]

def run(cmd, cwd=None, check=True):
    print(f"$ {' '.join(cmd)}", file=sys.stderr)
    result = subprocess.run(cmd, cwd=cwd, text=True, capture_output=True)
    if result.stdout:
        print(result.stdout, end="")
    if result.stderr:
        print(result.stderr, end="", file=sys.stderr)
    if check and result.returncode != 0:
        raise subprocess.CalledProcessError(result.returncode, cmd, result.stdout, result.stderr)
    return result

def _run_npx(subcommand, label):
    """Run a tree-sitter-cli subcommand via npx; fail closed when npx is absent.

    Unlike a plain ``run`` call, a missing npx is mapped to a clear error and
    exit 1 instead of an uncaught FileNotFoundError traceback. The caller uses
    ``--skip-generate`` only when it deliberately wants to bypass the gate.
    """
    try:
        run(["npx", *subcommand], cwd=GRAMMAR_DIR)
    except FileNotFoundError:
        print(
            f"error: npx not found — cannot run tree-sitter {label}; "
            "install Node.js/tree-sitter-cli or pass --skip-generate",
            file=sys.stderr,
        )
        sys.exit(1)


def ensure_grammar_repo():
    if (GRAMMAR_DIR / ".git").exists():
        return
    print(f"Initializing new git repo in {GRAMMAR_DIR}")
    run(["git", "init"], cwd=GRAMMAR_DIR)
    run(["git", "branch", "-M", "main"], cwd=GRAMMAR_DIR)
    # Minimal initial commit if needed

def get_head_sha(cwd=GRAMMAR_DIR):
    r = run(["git", "rev-parse", "HEAD"], cwd=cwd)
    return r.stdout.strip()

def is_dirty(cwd=GRAMMAR_DIR):
    r = run(["git", "status", "--porcelain"], cwd=cwd)
    return bool(r.stdout.strip())

def update_extension_toml(new_rev: str, dry_run: bool = False):
    text = EXT_TOML.read_text(encoding="utf-8")
    # Replace rev = "..."
    # Find [grammars.rsc] section and rev line
    new_text, n = re.subn(
        r'(\[grammars\.rsc\][^\[]*?rev\s*=\s*")[^"]+(")',
        rf'\g<1>{new_rev}\g<2>',
        text,
        flags=re.DOTALL,
    )
    if n == 0:
        # Fallback: simple rev line. Anchored to SHA-like values only so a
        # `rev` in ANY other section of extension.toml is never rewritten.
        new_text, n = re.subn(r'rev\s*=\s*"[0-9a-f]{7,40}"', f'rev = "{new_rev}"', text)
    if n == 0:
        print("error: could not find rev in extension.toml", file=sys.stderr)
        sys.exit(1)
    if text == new_text:
        print("extension.toml rev already up to date")
        return
    if dry_run:
        # Never touch the file during dry-run: writing here would destroy
        # uncommitted local modifications on revert.
        print(f'DRY-RUN: would set rev = "{new_rev}"')
        return
    EXT_TOML.write_text(new_text, encoding="utf-8")
    print(f"Updated {EXT_TOML} rev -> {new_rev}")

def main():
    p = argparse.ArgumentParser(description="Publish grammar and update extension.toml rev")
    p.add_argument("--dry-run", action="store_true", help="Show actions without pushing")
    p.add_argument("--push", action="store_true", help="Actually push to remote")
    p.add_argument("--remote", default="origin", help="Git remote name (default origin). Use 'grammar-bare' for local bare repo")
    p.add_argument("--remote-url", default="https://github.com/balakar94/tree-sitter-rsc", help="Remote URL if not yet added")
    p.add_argument("--skip-generate", action="store_true", help="Skip the tree-sitter generate + corpus gates (explicit bypass)")
    p.add_argument("--no-commit", action="store_true", help="Do not auto-commit: stage nothing, print manual git add/commit steps instead")
    p.add_argument("--branch", default="main", help="Target branch on the remote (default: main)")
    args = p.parse_args()

    if not GRAMMAR_DIR.exists():
        print(f"error: {GRAMMAR_DIR} not found", file=sys.stderr)
        sys.exit(1)

    if args.dry_run and not (GRAMMAR_DIR / ".git").exists():
        # Strictly side-effect free: report what a real run would do instead of
        # initializing the repo (ensure_grammar_repo would run `git init`).
        print(f"DRY-RUN: {GRAMMAR_DIR} is not a git repo; would run 'git init' and first commit")
        print(f"DRY-RUN: would push HEAD:{args.branch} to {args.remote} and update {EXT_TOML} rev")
        return

    ensure_grammar_repo()

    # Ensure remote exists
    remotes = run(["git", "remote"], cwd=GRAMMAR_DIR).stdout.split()
    if args.remote not in remotes:
        if args.remote == "grammar-bare":
            url = str(BARE_REPO)
        else:
            url = args.remote_url
        print(f"Adding remote {args.remote} -> {url}")
        if not args.dry_run:
            run(["git", "remote", "add", args.remote, url], cwd=GRAMMAR_DIR)

    # Fail-closed publish gates, part 1: generation must succeed and the corpus
    # must pass before anything is committed or pushed. The generated-output
    # freshness diff runs as part 2 AFTER the commit below: a dirty pre-commit
    # tree legitimately differs from HEAD, so diffing here would reject every
    # real grammar change.
    if not args.skip_generate and (GRAMMAR_DIR / "grammar.js").exists():
        if args.dry_run:
            # Dry-run must be side-effect free: generating would overwrite
            # grammars/rsc/src/* (generated outputs).
            print("DRY-RUN: would run 'npx tree-sitter generate'")
            print("DRY-RUN: would run 'npx tree-sitter test'")
        else:
            print("Running tree-sitter generate...")
            _run_npx(["tree-sitter", "generate"], "generate")
            print("Running tree-sitter corpus tests before publish...")
            _run_npx(["tree-sitter", "test"], "test")

    # Check dirty: dry-run stays side-effect free and reports first.
    if is_dirty():
        print(f"Grammar repo has uncommitted changes in {GRAMMAR_DIR}:")
        run(["git", "status", "--short"], cwd=GRAMMAR_DIR, check=False)
        staged = [p for p in GRAMMAR_ALLOWLIST if (GRAMMAR_DIR / p).exists()]
        if args.dry_run:
            print(f"DRY-RUN: would stage allowlist only: git add -- {' '.join(staged)}")
            print("DRY-RUN: would commit and push (no changes made)")
        elif args.no_commit:
            print("Not auto-committing (--no-commit). To publish manually:")
            print(f"  git -C {GRAMMAR_DIR} add -- {' '.join(staged)}")
            print('  git -C {0} commit -m "chore: publish grammar"'.format(GRAMMAR_DIR))
            print("Then re-run with --push (without --no-commit) or push manually.")
        else:
            print("Staging allowlisted grammar sources only...")
            run(["git", "add", "--", *staged], cwd=GRAMMAR_DIR)
            # Ensure user config exists
            try:
                run(["git", "config", "user.name"], cwd=GRAMMAR_DIR)
            except subprocess.CalledProcessError:
                run(["git", "config", "user.name", "publish-grammar"], cwd=GRAMMAR_DIR)
                run(["git", "config", "user.email", "publish@mikrotik-zed"], cwd=GRAMMAR_DIR)
            run(["git", "commit", "-m", "chore: publish grammar"], cwd=GRAMMAR_DIR)
    else:
        print("Grammar repo clean, no commit needed")

    # Fail-closed publish gates, part 2: re-generate and assert zero drift
    # against the committed outputs. Catches a stale parser.c that was not
    # regenerated and non-deterministic generation before anything is pushed.
    # Skipped with --no-commit (the tree intentionally stays dirty then).
    if (
        not args.skip_generate
        and (GRAMMAR_DIR / "grammar.js").exists()
        and not is_dirty()
    ):
        if args.dry_run:
            print(
                "DRY-RUN: would re-run 'npx tree-sitter generate' and assert no "
                "generated-output drift"
            )
        else:
            print("Checking generated-output freshness against the commit...")
            _run_npx(["tree-sitter", "generate"], "generate")
            freshness = run(
                [
                    "git",
                    "diff",
                    "--exit-code",
                    "--",
                    "src/parser.c",
                    "src/grammar.json",
                    "src/node-types.json",
                ],
                cwd=GRAMMAR_DIR,
                check=False,
            )
            if freshness.returncode != 0:
                print(
                    "error: generated outputs differ from the committed revision after "
                    "'tree-sitter generate' — regenerate and commit grammars/rsc/src/* "
                    "before publishing",
                    file=sys.stderr,
                )
                sys.exit(1)

    head = get_head_sha()
    print(f"Grammar HEAD: {head}")

    if args.dry_run:
        print(f"DRY-RUN: would push HEAD:{args.branch} to {args.remote} and update extension.toml rev to {head}")
        update_extension_toml(head, dry_run=True)
        return

    if not args.push:
        print("Not pushing (use --push to push). Still updating extension.toml rev locally.")
        update_extension_toml(head)
        print(f"Next: git add extension.toml && git commit -m 'chore: bump grammar rev to {head[:7]}'")
        return

    print(f"Pushing to {args.remote} (HEAD -> {args.branch})...")
    run(["git", "push", args.remote, f"HEAD:{args.branch}"], cwd=GRAMMAR_DIR)

    new_head = get_head_sha()
    print(f"Pushed, new HEAD {new_head}")

    update_extension_toml(new_head)

    # grammars/rsc is untracked — nothing to stage in the parent repo.
    print("Parent repo stays untouched (grammar working copy is untracked).")
    print(f"Next: git commit -m 'chore: bump grammar rev to {new_head[:7]}' ({EXT_TOML})")

    # Cargo.lock handling: ensure committed for binary workspaces
    # Run cargo generate-lockfile if needed, but don't ignore
    if (ROOT / "Cargo.lock").exists():
        print("Cargo.lock exists and should be committed (binary workspace)")
    else:
        print("warn: Cargo.lock missing, consider `cargo generate-lockfile` and committing it")

    # Suggest version bump check
    print("Done. Verify:")
    print(f"  git -C {GRAMMAR_DIR} log --oneline -1")
    print(f"  grep rev {EXT_TOML}")
    print("If you bumped grammars/rsc version, also bump root Cargo.toml, lsp/Cargo.toml, extension.toml version together.")

if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as e:
        # Single choke point: run() has already streamed the failing command's
        # output, so just report it compactly instead of dumping a traceback.
        cmd = " ".join(e.cmd) if isinstance(e.cmd, (list, tuple)) else str(e.cmd)
        returncode = e.returncode if isinstance(e.returncode, int) else -1
        print(f"error: command '{cmd}' failed with exit code {returncode}", file=sys.stderr)
        sys.exit(1)
