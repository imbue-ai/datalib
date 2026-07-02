#!/usr/bin/env bash
# Build the Frankweiler desktop app and launch it — one command.
#
#   ./run.sh                # native folder picker chooses the data root
#   ./run.sh ~/my-root      # skip the picker, boot straight into a root
#
# `tauri build --debug` runs the config's beforeBuildCommand (bazel
# doltlite archive + pnpm UI bundle) and produces the .app, so there are
# no prerequisite steps to remember. We then launch it with `open` so
# macOS treats it as a real app (see the launch block below for why that
# matters). A data-root argument is forwarded to skip the folder picker
# (see `explicit_data_root` in src/main.rs), which also makes the app
# scriptable/testable without a GUI click.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"

# pnpm on PATH via corepack, same shim the other dev scripts use.
# shellcheck source=../../scripts/ensure_pnpm.sh
source "$here/../../scripts/ensure_pnpm.sh"

# Incremental: recompiles only what changed, re-runs the (bazel-cached)
# beforeBuildCommand, re-bundles. Seconds when nothing changed.
pnpm dlx @tauri-apps/cli@2 build --debug

# The in-process sync worker shells out to `frankweiler-sync`. A bundle
# launched via `open` gets a stripped launchd environment, so exporting
# $FRANKWEILER_SYNC_BIN here wouldn't reach it — we hand it in explicitly
# with `open --env` below. Resolve it from Bazel (same binary dev.sh /
# serve_dev.sh use) unless the caller already pinned one. It's a single
# self-contained executable (no `data` deps, so its runfiles tree is
# empty), so pointing at the bazel-bin path directly is enough.
if [[ -z "${FRANKWEILER_SYNC_BIN:-}" ]]; then
  bazelisk build //frankweiler/backend/sync:frankweiler_sync_bin
  sync_bin="$(bazelisk info bazel-bin)/frankweiler/backend/sync/frankweiler_sync_bin"
  [[ -x "$sync_bin" ]] && export FRANKWEILER_SYNC_BIN="$sync_bin"
fi
open_env_args=()
[[ -n "${FRANKWEILER_SYNC_BIN:-}" ]] &&
  open_env_args+=(--env "FRANKWEILER_SYNC_BIN=$FRANKWEILER_SYNC_BIN")

# macOS: launch through LaunchServices (`open`), NOT by exec'ing the
# binary inside the bundle. A bundle binary run directly gets NSBundle
# context (so dialogs at least appear), but LaunchServices never
# registers it as a foreground app — the result is no Dock tile, no app
# activation (the cursor won't switch to I-beam over text fields, and
# the folder picker isn't key so clicking to navigate does nothing).
# `open` registers and activates it like a normal double-click.
#   -n          fresh instance (don't reattach to a stale one)
#   --args "$@" forward an optional data root (skips the picker)
# The in-process backend log goes to the unified log, not this terminal
# (a GUI app launched via LaunchServices is detached from the tty):
#   log stream --level info --predicate 'process == "frankweiler-tauri"'
# For inline backend logs while debugging, run the binary directly with
# a data root instead: `cargo run -- ~/root`.
app_bundle="$here/target/debug/bundle/macos/Frankweiler.app"
if [[ -d "$app_bundle" ]]; then
  exec open -n "$app_bundle" ${open_env_args[@]+"${open_env_args[@]}"} --args "$@"
fi

# Non-macOS: `tauri build` emits a plain binary and there is no
# bundle-registration problem, so run it directly.
exec "$here/target/debug/frankweiler-tauri" "$@"
