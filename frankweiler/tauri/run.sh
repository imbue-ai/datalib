#!/usr/bin/env bash
# Build the Frankweiler desktop app and launch it — one command.
#
#   ./run.sh                # native folder picker chooses the data root
#   ./run.sh ~/my-root      # skip the picker, boot straight into a root
#
# `tauri build --debug` runs the config's beforeBuildCommand (bazel
# doltlite archive + pnpm UI bundle) and produces the .app, so there are
# no prerequisite steps to remember. We then launch the binary *inside*
# the bundle: on macOS a bare `cargo run` binary has no app context, so
# the native folder picker never presents and the app spins — the same
# binary run from within `Frankweiler.app` gets real app context and the
# dialog works. A data-root argument is forwarded to skip the picker
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

# macOS: the in-bundle binary (real app context). Elsewhere `tauri
# build` emits a plain binary and there is no bundle-context problem.
app="$here/target/debug/bundle/macos/Frankweiler.app/Contents/MacOS/frankweiler-tauri"
[[ -x "$app" ]] || app="$here/target/debug/frankweiler-tauri"
exec "$app" "$@"
