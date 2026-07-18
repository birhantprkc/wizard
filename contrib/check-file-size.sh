#!/bin/sh
# File-size ratchet: fails, listing offenders, if any tracked .rs or .js
# file exceeds MAX_LINES lines. Runs from any directory inside the repo.
set -eu

# Ratchet: only ever lower this number (after splitting the biggest
# file) — never raise it to let a growing file pass.
MAX_LINES=5100

cd "$(git rev-parse --show-toplevel)"

offenders=$(
    git ls-files -- '*.rs' '*.js' |
        grep -v -e '^target/' -e '^gui/assets/fonts/' -e '^Cargo\.lock$' |
        while IFS= read -r f; do
            lines=$(wc -l <"$f")
            if [ "$lines" -gt "$MAX_LINES" ]; then
                printf '%7d %s\n' "$lines" "$f"
            fi
        done
)

if [ -n "$offenders" ]; then
    echo "error: files exceed the $MAX_LINES-line ratchet:" >&2
    printf '%s\n' "$offenders" >&2
    echo "Split the offending file(s); do not raise MAX_LINES." >&2
    exit 1
fi
