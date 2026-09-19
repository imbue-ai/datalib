#!/usr/bin/env bash
# Sourced by the dev launchers (serve_dev.sh, dev.sh) after the runfiles
# bootstrap: stage a `DATALIB_RUNTIME_DIR` tree out of the Bazel-managed
# Node and the qmd + latchkey package stores, so a sync started from the
# dev UI runs both tools from the same lockfile-pinned trees the release
# ships — never through `npx`, which the binaries refuse without
# `DATALIB_ALLOW_NPX=1`.
#
# Symlinks, not copies: the stores live in bazel-bin and outlive this
# run. The layout is the one `datalib_runtime::node_runtime` resolves
# (and `scripts/stage_runtime.sh` builds for real):
#
#   <stage>/node/bin/node
#   <stage>/qmd/<v>/node_modules       -> the qmd store
#   <stage>/latchkey/<v>/node_modules  -> the latchkey store
#
# A caller's own `DATALIB_RUNTIME_DIR` wins. Requires the launching
# rule to carry `_DEV_RUNTIME_DATA` / `_DEV_RUNTIME_ENV` from
# datalib/BUILD.bazel; a missing input is fatal rather than a silent
# fall-through, for the same reason run_e2e.sh makes it fatal.
#
# Also names the checkout's commit for the binaries (`DATALIB_GIT_HASH`,
# read by `datalib_runs::git_hash`), so the log view can link a line to
# its source. A dev build is never stamped, and uncommitted edits make
# the link approximate — which is still better than none.

if [[ -z "${DATALIB_GIT_HASH:-}" && -n "${BUILD_WORKSPACE_DIRECTORY:-}" ]]; then
  if DATALIB_GIT_HASH="$(git -C "$BUILD_WORKSPACE_DIRECTORY" rev-parse HEAD 2>/dev/null)"; then
    export DATALIB_GIT_HASH
    echo "git hash: $DATALIB_GIT_HASH"
  else
    unset DATALIB_GIT_HASH
  fi
fi

if [[ -n "${DATALIB_RUNTIME_DIR:-}" ]]; then
  echo "runtime dir: $DATALIB_RUNTIME_DIR (caller-supplied)"
  return 0
fi

_rt_need() { # env var holding an rlocationpath, human name
  local path
  path="$(rlocation "${!1:-}")" || path=""
  if [[ -z "$path" || ! -e "$path" ]]; then
    echo "ERROR: $2 not in runfiles ($1='${!1:-}')" >&2
    echo "Did it drop out of _DEV_RUNTIME_DATA / _DEV_RUNTIME_ENV in datalib/BUILD.bazel?" >&2
    exit 1
  fi
  printf '%s' "$path"
}

_rt_node="$(_rt_need DATALIB_DEV_NODE_BIN_RLOC "bazel-managed node")"
_rt_qmd_pkg="$(_rt_need DATALIB_DEV_QMD_PKG_RLOC "qmd runtime package.json")"
_rt_qmd_dir="$(_rt_need DATALIB_DEV_QMD_DIR_RLOC "qmd package dir")"
_rt_latchkey_pkg="$(_rt_need DATALIB_DEV_LATCHKEY_PKG_RLOC "latchkey runtime package.json")"
_rt_latchkey_dir="$(_rt_need DATALIB_DEV_LATCHKEY_DIR_RLOC "latchkey package dir")"

# The versions name the staged directories and the resolver looks them
# up by the Rust constants; //tools:version_pins_test holds each equal
# to its package.json, so read them from there rather than spelling the
# numbers out again.
_rt_pin() { # package.json, dependency name
  sed -n 's/.*"'"$2"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$1"
}
_rt_qmd_version="$(_rt_pin "$_rt_qmd_pkg" '@tobilu\/qmd')"
_rt_latchkey_version="$(_rt_pin "$_rt_latchkey_pkg" 'latchkey')"
[[ -n "$_rt_qmd_version" && -n "$_rt_latchkey_version" ]] \
  || { echo "ERROR: could not read the qmd/latchkey pins from the runtime package.json files" >&2; exit 1; }

# `<store>/<pkg>` -> `<store>`: the package dir is a link INSIDE the
# pnpm store, and the tools resolve their dependencies from siblings in
# it, so the whole store is what gets linked.
_rt_qmd_store="$(cd "$_rt_qmd_dir/../.." && pwd)"
_rt_latchkey_store="$(cd "$_rt_latchkey_dir/.." && pwd)"

_rt_stage="$(mktemp -d -t datalib-runtime.XXXXXX)"
mkdir -p "$_rt_stage/node/bin" "$_rt_stage/qmd/$_rt_qmd_version" "$_rt_stage/latchkey/$_rt_latchkey_version"
ln -sfn "$_rt_node" "$_rt_stage/node/bin/node"
ln -sfn "$_rt_qmd_store" "$_rt_stage/qmd/$_rt_qmd_version/node_modules"
ln -sfn "$_rt_latchkey_store" "$_rt_stage/latchkey/$_rt_latchkey_version/node_modules"

for _rt_entry in \
  "$_rt_stage/qmd/$_rt_qmd_version/node_modules/@tobilu/qmd/dist/cli/qmd.js" \
  "$_rt_stage/latchkey/$_rt_latchkey_version/node_modules/latchkey/dist/src/cli.js"; do
  [[ -f "$_rt_entry" ]] || { echo "ERROR: staged entry missing: $_rt_entry" >&2; exit 1; }
done

export DATALIB_RUNTIME_DIR="$_rt_stage"
echo "runtime dir: $DATALIB_RUNTIME_DIR (bazel-managed node + qmd@$_rt_qmd_version + latchkey@$_rt_latchkey_version)"
