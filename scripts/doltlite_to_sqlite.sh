#!/bin/bash
# Convert a DoltLite database to a SQLite database at the same path with a
# .sqlite suffix, by dumping it to a temp SQL file and re-ingesting it.
# Read-only, so it is safe against a store a sync is writing
# (docs/dev/doltlite.md § Getting the data out).
set -euo pipefail

src="$1"
dst="${src%.*}.sqlite"
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

doltlite -readonly "$src" '.dump' > "$tmp"
rm -f "$dst"
sqlite3 "$dst" < "$tmp"

echo "Wrote $dst"
