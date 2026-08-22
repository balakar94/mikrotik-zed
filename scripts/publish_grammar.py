#!/usr/bin/env python3
"""
Publish the grammar submodule (grammars/rsc -> balakar94/tree-sitter-rsc)
and update extension.toml rev + the parent-side submodule pointer.

By default uses the repo at https://github.com/balakar94/tree-sitter-rsc.
For local dev, can push to the bare repo at grammar-bare.git.

Usage:
  python scripts/publish_grammar.py --dry-run   # show what would be done
  python scripts/publish_grammar.py --push      # push to remote + update rev
  python scripts/publish_grammar.py --push --remote grammar-bare  # local bare

Steps:
  1) Ensure grammars/rsc is a git repo (init if needed)
  2) Check tree-sitter generate freshness (optional)
  3) Commit any changes in grammars/rsc (if dirty)
  4) Push to remote (origin / grammar-bare)
  5) Get new HEAD SHA
  6) Update extension.toml [grammars.rsc].rev
  7) (Optional) bump Cargo.lock handling: run cargo generate-lockfile, keep Cargo.lock committed

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
GITMODULES = ROOT / ".gitmodules"
# Submodule path as registered in .gitmodules (see ensure_grammar_repo).
GRAMMAR_SUBMODULE_PATH = "grammars/rsc"

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

def _is_registered_submodule() -> bool:
    """Check .gitmodules for a 'path = grammars/rsc' entry."""
    if not GITMODULES.exists():
        return False
    try:
        text = GITMODULES.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return False
    return any(line.strip() == f"path = {GRAMMAR_SUBMODULE_PATH}" for line in text.splitlines())

def ensure_grammar_repo():
    if (GRAMMAR_DIR / ".git").exists():
        return
    # A missing grammars/rsc/.git is exactly the state after a fresh clone
    # without `git submodule update --init`. Running `git init` there would
    # silently turn the empty submodule directory into an unrelated repo, so
    # refuse with an actionable message instead.
    if _is_registered_submodule():
        print(
            f"error: {GRAMMAR_SUBMODULE_PATH} is a registered git submodule but is not initialized;"
            " run: git submodule update --init --recursive",
            file=sys.stderr,
        )
        sys.exit(1)
    # Genuinely not a submodule (local/bare layout): keep the plain-init fallback.
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
    p.add_argument("--skip-generate", action="store_true", help="Skip tree-sitter generate check")
    args = p.parse_args()

    if not GRAMMAR_DIR.exists():
        print(f"error: {GRAMMAR_DIR} not found", file=sys.stderr)
        sys.exit(1)

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

    # Optional: verify parser.c freshness
    if not args.skip_generate:
        if (GRAMMAR_DIR / "grammar.js").exists():
            if args.dry_run:
                # Dry-run must be side-effect free: generating would overwrite
                # grammars/rsc/src/* (generated outputs).
                print("DRY-RUN: would run 'npx tree-sitter generate'")
            else:
                print("Checking tree-sitter generate freshness...")
                try:
                    run(["npx", "tree-sitter", "generate"], cwd=GRAMMAR_DIR, check=False)
                except FileNotFoundError:
                    print("warn: npx not found, skipping generate", file=sys.stderr)

    # Check dirty
    if is_dirty():
        print(f"Grammar repo has uncommitted changes in {GRAMMAR_DIR}:")
        run(["git", "status", "--short"], cwd=GRAMMAR_DIR, check=False)
        if args.dry_run:
            print("DRY-RUN: would commit and push")
        else:
            # Auto commit? Ask or do
            print("Staging all changes...")
            run(["git", "add", "-A"], cwd=GRAMMAR_DIR)
            # Ensure user config exists
            try:
                run(["git", "config", "user.name"], cwd=GRAMMAR_DIR)
            except subprocess.CalledProcessError:
                run(["git", "config", "user.name", "publish-grammar"], cwd=GRAMMAR_DIR)
                run(["git", "config", "user.email", "publish@mikrotik-zed"], cwd=GRAMMAR_DIR)
            run(["git", "commit", "-m", "chore: publish grammar"], cwd=GRAMMAR_DIR)
    else:
        print("Grammar repo clean, no commit needed")

    head = get_head_sha()
    print(f"Grammar HEAD: {head}")

    if args.dry_run:
        print(f"DRY-RUN: would push to {args.remote} and update extension.toml rev to {head}")
        update_extension_toml(head, dry_run=True)
        return

    if not args.push:
        print("Not pushing (use --push to push). Still updating extension.toml rev locally.")
        update_extension_toml(head)
        print(f"Next: git add extension.toml && git commit -m 'chore: bump grammar rev to {head[:7]}'")
        return

    print(f"Pushing to {args.remote}...")
    run(["git", "push", args.remote, "HEAD:main"], cwd=GRAMMAR_DIR)

    new_head = get_head_sha()
    print(f"Pushed, new HEAD {new_head}")

    update_extension_toml(new_head)

    # Stage the submodule pointer bump in the parent repo
    run(["git", "-C", str(ROOT), "add", "grammars/rsc"], check=False)
    print("Staged submodule pointer bump ('grammars/rsc') in the parent repo.")
    print(f"Next: git commit -m 'chore: bump grammar submodule to {new_head[:7]}'")

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
