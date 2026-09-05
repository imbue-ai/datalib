#!/usr/bin/env bash
# Build two directory trees that differ in every way the viewer knows
# how to report, scan both with the real fsindex, and render the page.
#
#   ./demo.sh [outdir]
#
# Covers: a moved subtree, a moved file, an edited file, a genuine
# delete, a delete whose bytes survive elsewhere, a genuinely new file,
# and a copy of content that already existed.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"
out="${1:-/tmp/dirtree_diff_demo}"

fsindex="$repo/bazel-bin/datalib/backend/etl/providers/fsindex/fsindex"
doltlite="$repo/bazel-bin/third-party/doltlite/doltlite"
dirtree_diff="$repo/bazel-bin/datalib/backend/dirtree_diff/dirtree_diff"
if [[ ! -x "$fsindex" || ! -x "$doltlite" || ! -x "$dirtree_diff" ]]; then
    echo "building the scanner, the viewer and the doltlite shell…" >&2
    (cd "$repo" && bazelisk build \
        //datalib/backend/etl/providers/fsindex:fsindex \
        //datalib/backend/dirtree_diff:dirtree_diff \
        //third-party/doltlite:doltlite)
fi

rm -rf "$out"
mkdir -p "$out"
cd "$out"

# ---- the "before" tree ----
mkdir -p before/docs/reports before/src/core before/media
echo "annual report body"  > before/docs/reports/annual.txt
echo "q3 numbers"          > before/docs/reports/q3.txt
echo "fn main() {}"        > before/src/core/main.rs
echo "helper code"         > before/src/core/util.rs
echo "shared boilerplate"  > before/src/core/license_header.txt
echo "shared boilerplate"  > before/docs/license_header.txt   # a duplicate
echo "readme v1"           > before/README.md
echo "photo bytes"         > before/media/pic.bin
echo "logo bytes"          > before/media/logo.bin
mkdir -p before/themes/dark
echo "dark base"           > before/themes/dark/base.css
echo "dark accents"        > before/themes/dark/accents.css

# ---- the "after" tree ----
cp -R before after
# a whole subtree moves
mkdir -p after/archive && mv after/docs/reports after/archive/reports
# a single file moves
mv after/media/logo.bin after/media/brand_logo.bin
# an edit
echo "readme v2, rewritten" > after/README.md
# a real delete: these bytes exist nowhere else
rm after/src/core/util.rs
# a delete whose bytes survive: the sibling duplicate stays
rm after/docs/license_header.txt
# genuinely new content
echo "brand new module"     > after/src/core/new_mod.rs
# a copy of content that already existed on the left
cp after/media/pic.bin after/media/pic_backup.bin
# a whole directory copied wholesale: should collapse to one row
cp -R after/themes/dark after/themes/dark_backup

echo "scanning…" >&2
"$fsindex" --db before.doltlite_db --root before --no-stamp >/dev/null 2>&1
"$fsindex" --db after.doltlite_db  --root after  --no-stamp >/dev/null 2>&1

# Case 1: two independent files, unified through file:// remotes.
"$dirtree_diff" \
    --left "$out/before.doltlite_db" \
    --right "$out/after.doltlite_db" \
    --full-tree \
    --dup-threshold 1 \
    -o "$out/diff_two_files.html"

# Case 2: the same two roots as two branches of ONE file, scanned
# straight into it with `fsindex --branch`, then diffed directly — no
# unification needed, because both commits already share a chunk store.
rm -f branched.doltlite_db
"$fsindex" --db branched.doltlite_db --root before --branch before --no-stamp >/dev/null 2>&1
"$fsindex" --db branched.doltlite_db --root after --branch after --no-stamp >/dev/null 2>&1

"$dirtree_diff" \
    --left "$out/branched.doltlite_db#before" \
    --right "$out/branched.doltlite_db#after" \
    --full-tree \
    --dup-threshold 1 \
    -o "$out/diff_two_branches.html"

echo
echo "sizes — one file holding both scans vs. two separate ones:"
ls -l before.doltlite_db after.doltlite_db branched.doltlite_db | awk '{printf "  %-26s %10s\n", $NF, $5}'

echo
echo "open:"
echo "  $out/diff_two_files.html      (two separate .doltlite_db files)"
echo "  $out/diff_two_branches.html   (two branches of one file)"
