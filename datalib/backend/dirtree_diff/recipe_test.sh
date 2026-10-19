#!/usr/bin/env bash
# Runs the code blocks of README.md §"A worked recipe on a Mac" as
# written, against a made-up tree, and checks the answers they print.
# zsh runs them when it is installed (with EXTENDED_GLOB, the trap the
# recipe quotes for), bash otherwise.
set -euo pipefail

readme=$1 fsindex=$2 dirtree_diff=$3 doltlite=$4

work=$(mktemp -d "${TEST_TMPDIR:-/tmp}/recipe.XXXXXX")
mkdir -p "$work/bin" "$work/tmp"
ln -s "$PWD/$fsindex" "$work/bin/datalib-fsindex"
ln -s "$PWD/$dirtree_diff" "$work/bin/datalib-dirtree-diff"
ln -s "$PWD/$doltlite" "$work/bin/datalib-doltlite"
# The recipe ends by opening the page; here that only has to be a page.
printf '#!/bin/sh\ntest -s "$1"\n' > "$work/bin/open"
chmod +x "$work/bin/open"

recipe="$work/recipe.sh"
awk '/^## A worked recipe on a Mac/ { on = 1; next }
     on && /^## / { exit }
     on && /^```zsh$/ { code = 1; next }
     code && /^```$/ { code = 0; next }
     code' "$readme" > "$recipe"
grep -q datalib-dirtree-diff "$recipe" || {
  echo "found no recipe in $readme:" >&2
  cat "$recipe" >&2
  exit 1
}

export HOME="$work/home" TMPDIR="$work/tmp/" DATALIB_CACHE_DIR="$work/cache"
export PATH="$work/bin:$PATH"

# The two folders the recipe names, filled with a move, a deletion and an
# addition.
eval "$(grep -E '^(old|new)=' "$recipe")"
mkdir -p "$old/reports" "$new/archive/reports"
echo picard > "$old/reports/a.txt"
echo picard > "$new/archive/reports/a.txt"
echo riker > "$old/b.txt"
echo riker > "$new/b.txt"
echo troi > "$old/gone.txt"
echo data > "$new/new.txt"

out="$work/out"
if command -v zsh > /dev/null; then
  zsh -f -e -o extendedglob "$recipe" > "$out" 2>&1 || status=$?
else
  bash -e "$recipe" > "$out" 2>&1 || status=$?
fi
if [ "${status:-0}" -ne 0 ]; then
  echo "the recipe failed ($status):" >&2
  cat "$out" >&2
  exit 1
fi

expect() {
  grep -Eq "$1" "$out" || {
    echo "expected /$1/ in the recipe's output:" >&2
    cat "$out" >&2
    exit 1
  }
}
expect '1 move\(s\).* 1 deleted, 0 deleted-with-copy-remaining'
# The SQL check: one file (gone.txt, 5 bytes) is nowhere in the newer tree.
expect '│ +1 │ +5 │'
echo ok
