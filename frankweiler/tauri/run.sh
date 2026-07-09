#!/usr/bin/env bash
# Build the Frankweiler desktop app and launch it — one command.
#
#   ./run.sh                # native folder picker chooses the data root
#   ./run.sh ~/my-root      # skip the picker, boot straight into a root
#
# `tauri build --debug` runs the config's beforeBuildCommand (bazel
# builds `frankweiler-http` — UI embedded — and `frankweiler-sync`,
# then stages both into binaries/ so they're bundled under the .app's
# Resources) and produces the .app, so there are no prerequisite steps
# to remember. The shell spawns the bundled `frankweiler-http` at
# runtime (see `resolve_http_bin` in src/main.rs). We then launch it
# with `open` so macOS treats it as a real app (see the launch block
# below for why that matters). A data-root argument is forwarded to
# skip the folder picker (see `explicit_data_root` in src/main.rs),
# which also makes the app scriptable/testable without a GUI click.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"

# pnpm on PATH via corepack, same shim the other dev scripts use.
# shellcheck source=../../scripts/ensure_pnpm.sh
source "$here/../../scripts/ensure_pnpm.sh"

# Incremental: recompiles only what changed, re-runs the (bazel-cached)
# beforeBuildCommand, re-bundles. Seconds when nothing changed.
pnpm dlx @tauri-apps/cli@2 build --debug

# macOS: launch through LaunchServices (`open`), NOT by exec'ing the
# binary inside the bundle. A bundle binary run directly gets NSBundle
# context (so dialogs at least appear), but LaunchServices never
# registers it as a foreground app — the result is no Dock tile, no app
# activation (the cursor won't switch to I-beam over text fields, and
# the folder picker isn't key so clicking to navigate does nothing).
# `open` registers and activates it like a normal double-click.
#   -n          fresh instance (don't reattach to a stale one)
#   --args "$@" forward an optional data root (skips the picker)
# The spawned backend's output goes to a log file in $TMPDIR
# (frankweiler-http-<pid>.log — see `start_backend` in src/main.rs).
# For inline shell logs while debugging, run the binary directly with
# a data root instead: `cargo run -- ~/root`.
app_bundle="$here/target/debug/bundle/macos/Frankweiler.app"
if [[ -d "$app_bundle" ]]; then
  exec open -n "$app_bundle" --args "$@"
fi

# Non-macOS: `tauri build` emits a plain binary and there is no
# bundle-registration problem, so run it directly.
exec "$here/target/debug/frankweiler-tauri" "$@"
