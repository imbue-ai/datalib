#!/usr/bin/env bash
# Render the chat-common sample corpus to markdown, then to one
# interactive HTML page — the checked-in golden
# `datalib/ui/tests/goldens/chat_preview.html`, which is there so a chat
# layout change can be reviewed by opening a file in a browser.
#
#   bazelisk test //datalib/ui:chat_preview_test   # regenerate and diff
#   bazelisk run  //datalib/ui:chat_preview        # rewrite the golden
#
# The two modes differ only in where the CSS comes from and what happens
# to the result: `run` reads the .vue files from your working tree (so an
# edit shows up with no rebuild) and overwrites the golden; `test` reads
# the copies bazel staged and diffs.
set -euo pipefail

f=bazel_tools/tools/bash/runfiles/runfiles.bash
# shellcheck disable=SC1090
source "${RUNFILES_DIR:-/dev/null}/$f" 2>/dev/null \
  || source "$(grep -sm1 "^$f " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f 2- -d ' ')" 2>/dev/null \
  || source "$0.runfiles/$f" 2>/dev/null \
  || source "$0.runfiles/_main/$f" 2>/dev/null \
  || { echo>&2 "ERROR: cannot find bazel runfiles bootstrap"; exit 1; }

# Resolve a runfile or die naming it: every one of these is a `data` dep
# of both targets, so a miss means the dep is gone, not that it's optional.
need_runfile() {
  local out=""
  out="$(rlocation "$1")" || out=""
  if [[ -z "$out" || ! -e "$out" ]]; then
    echo "ERROR: cannot locate '$1' in runfiles" >&2
    exit 1
  fi
  echo "$out"
}

samples_bin="$(need_runfile "$FW_SAMPLES_BIN_RLOC")"
node_bin="$(need_runfile "$FW_NODE_BIN_RLOC")"
preview_js="$(need_runfile "$FW_PREVIEW_JS_RLOC")"
golden="$(need_runfile "$FW_GOLDEN_RLOC")"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
"$samples_bin" "$work/md" >/dev/null

# `--ui` names the tree the card CSS is read from; the script always
# resolves its own npm packages from beside itself in the runfiles.
if [[ -n "${BUILD_WORKSPACE_DIRECTORY:-}" ]]; then
  ui_root="$BUILD_WORKSPACE_DIRECTORY/datalib/ui"
else
  ui_root="$(dirname "$preview_js")/.."
fi

"$node_bin" "$preview_js" --md "$work/md" --out "$work/chat_preview.html" --ui "$ui_root"

if [[ -n "${BUILD_WORKSPACE_DIRECTORY:-}" ]]; then
  dest="$BUILD_WORKSPACE_DIRECTORY/datalib/ui/tests/goldens/chat_preview.html"
  cp "$work/chat_preview.html" "$dest"
  echo "wrote $dest"
  exit 0
fi

if ! diff -u "$golden" "$work/chat_preview.html" > "$work/diff.txt"; then
  echo "The chat preview golden is stale." >&2
  echo "Regenerate it with:  bazelisk run //datalib/ui:chat_preview" >&2
  echo >&2
  head -c 8000 "$work/diff.txt" >&2
  exit 1
fi
