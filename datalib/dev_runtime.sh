#!/usr/bin/env bash
# Sourced by the dev launchers (serve_dev.sh, dev.sh) after the runfiles
# bootstrap: stage a `DATALIB_RUNTIME_DIR` tree out of the Bazel-managed
# Node and the latchkey package store, so a sync started from the dev UI
# runs latchkey from the same lockfile-pinned tree the release ships — never through `npx`, which the binaries refuse without
# `DATALIB_ALLOW_NPX=1`.
#
# Symlinks, not copies: the stores live in bazel-bin and outlive this
# run. The layout is the one `datalib_runtime::node_runtime` resolves
# (and `scripts/stage_runtime.sh` builds for real):
#
#   <stage>/node/bin/node
#   <stage>/latchkey/<v>/node_modules  -> the latchkey store
#
# A caller's own `DATALIB_RUNTIME_DIR` wins. Requires the launching
# rule to carry `DEV_RUNTIME_DATA` / `DEV_RUNTIME_ENV` from
# datalib/dev_runtime.bzl; a missing input is fatal rather than a silent
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
    echo "Did it drop out of DEV_RUNTIME_DATA / DEV_RUNTIME_ENV (datalib/dev_runtime.bzl)?" >&2
    exit 1
  fi
  printf '%s' "$path"
}

_rt_node="$(_rt_need DATALIB_DEV_NODE_BIN_RLOC "bazel-managed node")"
_rt_latchkey_pkg="$(_rt_need DATALIB_DEV_LATCHKEY_PKG_RLOC "latchkey runtime package.json")"
_rt_latchkey_dir="$(_rt_need DATALIB_DEV_LATCHKEY_DIR_RLOC "latchkey package dir")"

# The version names the staged directory and the resolver looks it up
# by the Rust constant; //tools:version_pins_test holds it equal to the
# package.json, so read it from there rather than spelling it out again.
_rt_pin() { # package.json, dependency name
  sed -n 's/.*"'"$2"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$1"
}
_rt_latchkey_version="$(_rt_pin "$_rt_latchkey_pkg" 'latchkey')"
[[ -n "$_rt_latchkey_version" ]] \
  || { echo "ERROR: could not read the latchkey pin from its runtime package.json" >&2; exit 1; }

# `<store>/<pkg>` -> `<store>`: the package dir is a link INSIDE the
# pnpm store, and the tool resolves its dependencies from siblings in
# it, so the whole store is what gets linked.
_rt_latchkey_store="$(cd "$_rt_latchkey_dir/.." && pwd)"

_rt_stage="$(mktemp -d -t datalib-runtime.XXXXXX)"
mkdir -p "$_rt_stage/node/bin" "$_rt_stage/latchkey/$_rt_latchkey_version"
ln -sfn "$_rt_node" "$_rt_stage/node/bin/node"
ln -sfn "$_rt_latchkey_store" "$_rt_stage/latchkey/$_rt_latchkey_version/node_modules"

_rt_entry="$_rt_stage/latchkey/$_rt_latchkey_version/node_modules/latchkey/dist/src/cli.js"
[[ -f "$_rt_entry" ]] || { echo "ERROR: staged entry missing: $_rt_entry" >&2; exit 1; }

export DATALIB_RUNTIME_DIR="$_rt_stage"
echo "runtime dir: $DATALIB_RUNTIME_DIR (bazel-managed node + latchkey@$_rt_latchkey_version)"
