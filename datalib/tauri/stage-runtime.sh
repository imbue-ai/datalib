#!/usr/bin/env bash
#
# Stage the bundled runtime into `datalib/tauri/runtime/` (shipped by
# tauri.conf.json under the .app's `Contents/Resources/runtime/`), the
# third-party notices into `datalib/tauri/licenses/`, put the
# user-facing `latchkey` launcher beside the sidecar binaries, and
# — on a signing build — codesign everything that will be notarized.
# The staging itself is `scripts/stage_runtime.sh`, shared with the
# release tarball; this file is only what the .app adds on top.
#
# Signing: when $APPLE_SIGNING_IDENTITY is set (same convention as
# tauri.conf.json's beforeBuildCommand), the node binary and every
# native library in the trees is codesigned with the hardened runtime.
# `node` additionally keeps the JIT entitlements extracted from the
# upstream-signed binary — V8 won't start under the hardened runtime
# without them.

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$script_dir/../.."
runtime_dir="$script_dir/runtime"

log() { printf '>>> stage-runtime: %s\n' "$*" >&2; }

"$repo_root/scripts/stage_runtime.sh" "$runtime_dir"

# The third-party notices, shipped under Contents/Resources/licenses/
# (tauri.conf.json lists the directory).
"$repo_root/scripts/third_party_notices.sh" "$script_dir/licenses"

# User-facing `latchkey` launcher: bundled node + staged tree +
# LATCHKEY_CURL pointed at the bundled shim. Lands next to the sidecar
# binaries (same dir the shim is staged into by beforeBuildCommand) so
# `.../Resources/binaries/latchkey services register …` just works.
mkdir -p "$script_dir/binaries"
install -m 0755 "$repo_root/scripts/latchkey-wrapper.sh" "$script_dir/binaries/latchkey"
log "installed latchkey wrapper at binaries/latchkey"

# ---------------------------------------------------------------------------
# Codesigning (macOS release builds only).
# ---------------------------------------------------------------------------

if [[ "$(uname -s)" == "Darwin" && -n "${APPLE_SIGNING_IDENTITY:-}" ]]; then
    log "codesigning runtime (identity: $APPLE_SIGNING_IDENTITY)"
    # Preserve the JIT entitlements the upstream node binary is signed
    # with — V8 aborts under the hardened runtime without them.
    entitlements="$(mktemp -t node-entitlements.XXXXXX)"
    if codesign -d --entitlements - --xml "$runtime_dir/node/bin/node" \
        >"$entitlements" 2>/dev/null && [[ -s "$entitlements" ]]; then
        codesign --force --options runtime --timestamp \
            --entitlements "$entitlements" \
            --sign "$APPLE_SIGNING_IDENTITY" "$runtime_dir/node/bin/node"
    else
        codesign --force --options runtime --timestamp \
            --sign "$APPLE_SIGNING_IDENTITY" "$runtime_dir/node/bin/node"
    fi
    rm -f "$entitlements"
    # Every native library in the trees must be signed for notarization.
    # *.so: node-llama-cpp names its Mach-O dylibs libggml-*.so.
    # `-type f` so the pnpm store's symlinks are signed once, through
    # the real file, rather than once per link.
    find "$runtime_dir/latchkey" "$runtime_dir/qmd" \
        \( -name '*.node' -o -name '*.dylib' -o -name '*.so' \) -type f -print0 |
        while IFS= read -r -d '' lib; do
            codesign --force --options runtime --timestamp \
                --sign "$APPLE_SIGNING_IDENTITY" "$lib"
        done
fi

log "runtime staged at $runtime_dir"
