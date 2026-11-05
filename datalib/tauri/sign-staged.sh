#!/usr/bin/env bash
#
# Codesign what `bundle.resources` ships, which Tauri's bundler leaves
# alone and notarization refuses unsigned: the staged binaries, the
# bundled node and every native library in the JS trees, all with the
# hardened runtime. Runs after `tauri build --no-bundle` has staged them
# and before `tauri bundle` copies them into the .app.
#
#   APPLE_SIGNING_IDENTITY=<sha1> datalib/tauri/sign-staged.sh
#
# Runs nothing out of the trees it signs.

set -euo pipefail

identity="${APPLE_SIGNING_IDENTITY:?the codesign identity to sign with}"
here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
binaries="$here/binaries"
runtime="$here/runtime"

sign() { codesign --force --options runtime --timestamp --sign "$identity" "$@"; }

sign "$binaries/datalib-http" "$binaries/datalib-dag" "$binaries/datalib-step" \
    "$binaries/datalib-applet" "$binaries/datalib-migrate-config" \
    "$binaries/latchkey-curl-router" "$binaries/curl-impersonate" \
    "$binaries/datalib-doltlite"

# Node keeps the JIT entitlements it was signed with upstream: V8 will
# not start under the hardened runtime without them.
entitlements="$(mktemp -t node-entitlements.XXXXXX)"
trap 'rm -f "$entitlements"' EXIT
if codesign -d --entitlements - --xml "$runtime/node/bin/node" >"$entitlements" 2>/dev/null \
    && [[ -s "$entitlements" ]]; then
    codesign --force --options runtime --timestamp --entitlements "$entitlements" \
        --sign "$identity" "$runtime/node/bin/node"
else
    sign "$runtime/node/bin/node"
fi

# *.so: node-llama-cpp names its Mach-O dylibs libggml-*.so.
find "$runtime/latchkey" "$runtime/qmd" \
    \( -name '*.node' -o -name '*.dylib' -o -name '*.so' \) -type f -print0 |
    while IFS= read -r -d '' lib; do
        sign "$lib"
    done

echo ">>> sign-staged: signed the staged binaries and runtime" >&2
