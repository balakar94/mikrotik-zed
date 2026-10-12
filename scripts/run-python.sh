#!/bin/sh
# Interpreter resolver for the Zed device tasks.
#
# The task definitions run this wrapper instead of a bare `python3` so the
# project venv wins when it exists: `make install-tools` installs `requests`
# and `paramiko` into `.venv`, while a bare `python3` on macOS is the system
# 3.9 interpreter, which has neither — the deploy tasks then exit 3 with a
# "missing dependency" message that hides the real cause. The fallback keeps
# the tasks working in checkouts that never ran `make install-tools`, and
# mirrors the documented Windows launcher (`python` when `python3` is absent).
#
# Usage: sh scripts/run-python.sh <script> [args...]   (also: `-c <code> args`)
set -eu

# Resolve the repo root from this script, so the wrapper works regardless of
# the caller's working directory.
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
venv_python="$root/.venv/bin/python"

if [ -x "$venv_python" ]; then
	exec "$venv_python" "$@"
fi

echo "note: $root/.venv not found - using the interpreter on PATH; run 'make install-tools' for the pinned dependencies" >&2

if command -v python3 >/dev/null 2>&1; then
	exec python3 "$@"
fi

exec python "$@"
