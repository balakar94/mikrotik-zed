#!/usr/bin/env bash
# Verify data/commands.toml is fresh, ignoring the non-deterministic
# '# Generated:' timestamp header. Expects the file to be already
# regenerated (e.g. via `make extract`); only compares content.
set -eu

FILE="data/commands.toml"

if [ ! -f "$FILE" ]; then
	echo "error: $FILE not generated" >&2
	exit 1
fi

# Handle shallow clones or rewritten history where HEAD has no copy yet.
if git cat-file -e "HEAD:$FILE" 2>/dev/null; then
	diff -u <(git show "HEAD:$FILE" | grep -v '^# Generated:') \
		<(grep -v '^# Generated:' "$FILE") ||
		{
			echo "$FILE stale — run 'make extract' and commit" >&2
			exit 1
		}
else
	echo "warning: HEAD:$FILE not found — checking file exists and is non-empty" >&2
	test -s "$FILE" || {
		echo "error: $FILE empty" >&2
		exit 1
	}
	grep -q '^\[\[menus\]\]' "$FILE" || {
		echo "error: $FILE missing [[menus]]" >&2
		exit 1
	}
fi

echo "commands.toml OK (timestamps ignored)"
