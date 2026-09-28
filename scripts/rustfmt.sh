#!/usr/bin/env bash
# `bazelisk run //:rustfmt -- <file or directory>...`: rewrites Rust files the
# way the rustfmt aspect in every `bazelisk build` checks them. A directory
# means every .rs file under it; an argument starting with `-` goes to rustfmt
# (`--check`). Exists because `@rules_rust//:rustfmt` takes labels, not paths,
# and splits them at spaces.
set -eo pipefail

# --- bazel runfiles bootstrap ---
f=bazel_tools/tools/bash/runfiles/runfiles.bash
# shellcheck disable=SC1090
source "${RUNFILES_DIR:-/dev/null}/$f" 2>/dev/null \
  || source "$(grep -sm1 "^$f " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f 2- -d ' ')" 2>/dev/null \
  || source "$0.runfiles/$f" 2>/dev/null \
  || source "$0.runfiles/_main/$f" 2>/dev/null \
  || { echo>&2 "ERROR: cannot find bazel runfiles bootstrap"; exit 1; }
set -u

rustfmt="$(rlocation "$RUSTFMT")"
config="$(rlocation "$RUSTFMT_CONFIG")"
cd "${BUILD_WORKING_DIRECTORY:?run this with bazelisk run //:rustfmt}"

flags=()
files=()
for arg in "$@"; do
  if [[ "$arg" == -* ]]; then
    flags+=("$arg")
  elif [[ -d "$arg" ]]; then
    while IFS= read -r -d '' file; do
      files+=("$file")
    done < <(find "$arg" \( -name node_modules -o -name target -o -name 'bazel-*' \) -prune \
      -o -name '*.rs' -type f -print0)
  elif [[ -f "$arg" ]]; then
    files+=("$arg")
  else
    echo "ERROR: no such file or directory: $arg" >&2
    exit 1
  fi
done

if [[ ${#files[@]} -eq 0 ]]; then
  echo "usage: bazelisk run //:rustfmt -- [--check] <file or directory>..." >&2
  exit 2
fi

# The same flags as rules_rust's rustfmt aspect, so what passes here passes there.
exec "$rustfmt" --config-path "$config" --edition "$RUST_EDITION" \
  --config skip_children=true ${flags[@]+"${flags[@]}"} "${files[@]}"
