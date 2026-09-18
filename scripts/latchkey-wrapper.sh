#!/bin/sh
# Thin launcher for the bundled latchkey CLI, shipped as `latchkey`
# beside the datalib binaries: in the .app's `Resources/binaries/` (with
# the runtime one level up), in the docker image (runtime a sibling
# directory) and in the release tarball (runtime fetched by the binaries
# into ~/.cache/datalib/runtime on first use — docs/dev/runtime_fetch.md).
# It runs the bundled Node + latchkey tree and points `LATCHKEY_CURL` at
# the router curl sitting next to it (which hands marked requests to
# the impersonator sibling) — so `latchkey curl`, `services register`
# and `auth set` work against Cloudflare-protected hosts with no Node,
# npm, or env setup on the host. An externally-set `LATCHKEY_CURL` wins;
# `DATALIB_RUNTIME_DIR` relocates the runtime tree (same override the
# Rust resolver honors, and the same lookup order).
#
# Same CLI as latchkey itself:
#   latchkey services list
set -eu

# Through any symlink first (install.sh links this into ~/.local/bin):
# the runtime sits beside the real file, not beside the link.
self="$0"
while [ -L "$self" ]; do
    target="$(readlink "$self")"
    case "$target" in
        /*) self="$target" ;;
        *) self="$(dirname -- "$self")/$target" ;;
    esac
done
here="$(cd -- "$(dirname -- "$self")" && pwd -P)"
runtime="${DATALIB_RUNTIME_DIR:-}"
if [ -z "$runtime" ]; then
    # The fetched tree last: the cache holds one release's runtime at a
    # time (the fetcher prunes the rest), so the glob matches at most
    # one directory.
    for candidate in "$here/runtime" "$here/../runtime" \
            "${XDG_CACHE_HOME:-$HOME/.cache}"/datalib/runtime/*; do
        if [ -x "$candidate/node/bin/node" ]; then
            runtime="$candidate"
            break
        fi
    done
    runtime="${runtime:-$here/runtime}"
fi
node="$runtime/node/bin/node"

if [ ! -x "$node" ]; then
    if [ -f "$here/runtime.manifest" ]; then
        echo "latchkey: the Node runtime has not been fetched yet — run \`datalib-step pull-runtime\` (a sync does it on its own)" >&2
    else
        echo "latchkey: bundled node not found at $node (scripts/stage_runtime.sh builds the runtime tree)" >&2
    fi
    exit 1
fi

# Exactly one latchkey tree is staged (stage_runtime.sh prunes stale
# versions); resolving by glob keeps this script free of a version pin
# of its own — the Rust sources stay the single source of truth.
entry=""
for candidate in "$runtime"/latchkey/*/node_modules/latchkey/dist/src/cli.js; do
    [ -f "$candidate" ] || continue
    if [ -n "$entry" ]; then
        echo "latchkey: multiple latchkey trees under $runtime/latchkey — re-run scripts/stage_runtime.sh to prune" >&2
        exit 1
    fi
    entry="$candidate"
done
if [ -z "$entry" ]; then
    echo "latchkey: no latchkey tree under $runtime/latchkey (run scripts/stage_runtime.sh)" >&2
    exit 1
fi

if [ -z "${LATCHKEY_CURL:-}" ] && [ -x "$here/latchkey-curl-router" ]; then
    LATCHKEY_CURL="$here/latchkey-curl-router"
    export LATCHKEY_CURL
fi

exec "$node" "$entry" "$@"
