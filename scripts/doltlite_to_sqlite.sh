#!/bin/bash
# Convert a DoltLite database to a SQLite database at the same path with a
# .sqlite suffix, by dumping it to a temp SQL file and re-ingesting it.
set -euo pipefail

src="$1"
dst="${src%.*}.sqlite"
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

doltlite "$src" '.dump' > "$tmp"
rm -f "$dst"
sqlite3 "$dst" < "$tmp"

echo "Wrote $dst"
