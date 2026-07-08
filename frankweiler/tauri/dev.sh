#!/usr/bin/env bash
# Dev-run the Frankweiler Tauri shell (debug build, folder picker, live
# window). Wraps the README's `pnpm dlx @tauri-apps/cli@^2 dev` with the
# prerequisites that plain `tauri dev` assumes already exist:
#
#   * bazel-bin/third-party/doltlite — `.cargo/config.toml` statically
#     links the Bazel-built doltlite archive (SQLITE3_LIB_DIR); without
#     it the cargo build fails at libsqlite3-sys.
#   * frankweiler-sync + the latchkey curl shim — the embedded backend's
#     sync worker shells out to `frankweiler-sync` (which in turn execs
#     the shim). Exported via $FRANKWEILER_SYNC_BIN / $FRANKWEILER_CURL_SHIM
#     (caller-supplied values are honored), same wiring as
#     ../serve_dev.sh; without them UI-triggered syncs fail with
#     "no frankweiler-sync binary found".
#   * frankweiler/ui/dist — the UI the window actually shows. The shell
#     opens its window at the in-process axum URL, which serves the
#     rust-embed'd dist (`debug-embed` bakes it in even for debug
#     builds). The Vite server that tauri's beforeDevCommand starts on
#     port 5173 is only the CLI's dev wait-gate — the window never
#     loads it, so UI edits need a re-run of this script, not a Vite
#     hot-reload.
#
# This crate is intentionally outside Bazel (see README.md); everything
# here is cargo/pnpm. Extra args are forwarded to `tauri dev`.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
repo_root="$(cd ../.. && pwd)"

bazelisk build \
    //third-party/doltlite:sqlite3 \
    //frankweiler/backend/sync:frankweiler_sync_bin \
    //frankweiler/backend/etl:latchkey_curl_shim

if [[ -z "${FRANKWEILER_SYNC_BIN:-}" ]]; then
    export FRANKWEILER_SYNC_BIN="${repo_root}/bazel-bin/frankweiler/backend/sync/frankweiler_sync_bin"
fi
if [[ -z "${FRANKWEILER_CURL_SHIM:-}" ]]; then
    export FRANKWEILER_CURL_SHIM="${repo_root}/bazel-bin/frankweiler/backend/etl/latchkey_curl_shim"
fi
echo "sync bin: $FRANKWEILER_SYNC_BIN"

pnpm --dir ../ui install --frozen-lockfile
pnpm --dir ../ui build

# tauri runs its beforeDevCommand (`pnpm --dir ui dev`) with
# frankweiler/ as the working directory, so Vite comes up on 5173 by
# itself; no need to start it here.
exec pnpm dlx @tauri-apps/cli@^2 dev "$@"
