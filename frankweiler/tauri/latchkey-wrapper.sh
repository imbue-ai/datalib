#!/bin/sh
# Thin launcher for the app-bundled latchkey CLI.
#
# stage-runtime.sh installs this as `binaries/latchkey`, next to the
# sidecar binaries, so it ships under the .app's `Resources/binaries/`.
# It runs the bundled Node + latchkey tree from `../runtime` and points
# `LATCHKEY_CURL` at the bundled dispatch curl sitting next to this script
# (which in turn routes marked requests to the impersonator sibling) — so
# `latchkey curl` (and `services register` /
# `auth set` during setup) work against Cloudflare-protected hosts with
# no Node, npm, or env setup on the host. An externally-set
# `LATCHKEY_CURL` wins; `FRANKWEILER_RUNTIME_DIR` relocates the runtime
# tree (same override the Rust resolver honors).
#
# Same CLI as latchkey itself:
#   .../Resources/binaries/latchkey services list
set -eu

here="$(cd -- "$(dirname -- "$0")" && pwd -P)"
runtime="${FRANKWEILER_RUNTIME_DIR:-$here/../runtime}"
node="$runtime/node/bin/node"

if [ ! -x "$node" ]; then
    echo "latchkey: bundled node not found at $node (run frankweiler/tauri/stage-runtime.sh)" >&2
    exit 1
fi

# Exactly one latchkey tree is staged (stage-runtime.sh prunes stale
# versions); resolving by glob keeps this script free of a version pin
# of its own — the Rust sources stay the single source of truth.
entry=""
for candidate in "$runtime"/latchkey/*/node_modules/latchkey/dist/src/cli.js; do
    [ -f "$candidate" ] || continue
    if [ -n "$entry" ]; then
        echo "latchkey: multiple latchkey trees under $runtime/latchkey — re-run stage-runtime.sh to prune" >&2
        exit 1
    fi
    entry="$candidate"
done
if [ -z "$entry" ]; then
    echo "latchkey: no latchkey tree under $runtime/latchkey (run frankweiler/tauri/stage-runtime.sh)" >&2
    exit 1
fi

if [ -z "${LATCHKEY_CURL:-}" ] && [ -x "$here/latchkey-curl-dispatch" ]; then
    LATCHKEY_CURL="$here/latchkey-curl-dispatch"
    export LATCHKEY_CURL
fi

exec "$node" "$entry" "$@"
