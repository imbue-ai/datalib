#!/usr/bin/env bash
# The `runtime` job of release.yml, minus the upload: stage the Node
# runtime for this host into `runtime/` (and `runtime-cuda/` when asked),
# prove node and qmd run out of it, and tar each tree into the asset the
# binaries fetch on first use (docs/dev/runtime_fetch.md). Writes
# `runtime-<triple>.tar.gz` (+ `-cuda`) and their `.sha256` sidecars into
# the current directory and prints those file names, one per line.
#
#   scripts/release/stage_runtime_asset.sh <triple> <cuda: true|false>
#
# A script rather than a `run:` block so the same lines run under
# //tools:stage_runtime_test — including on a mac's /bin/bash 3.2 —
# before a tag ever runs them. Inner scripts are run through "$BASH" so
# whichever bash runs this one runs them too.
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $0 <triple> <cuda: true|false>" >&2
    exit 2
fi
triple="$1"
cuda="$2"
[[ "$cuda" == "true" || "$cuda" == "false" ]] || { echo "cuda must be true or false, got '$cuda'" >&2; exit 2; }

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$script_dir/../.."

checksum() {
    if command -v sha256sum >/dev/null; then
        sha256sum "$1" > "$1.sha256"
    else
        shasum -a 256 "$1" > "$1.sha256"
    fi
}

# Two branches rather than an optional-args array: bash 3.2 treats an
# empty array as unbound under -u.
if [[ "$cuda" == "true" ]]; then
    "$BASH" "$repo_root/scripts/stage_runtime.sh" runtime --cuda runtime-cuda
else
    "$BASH" "$repo_root/scripts/stage_runtime.sh" runtime
fi

# Smoke the staged tree on this host (the same triple as the asset):
# the Node runs, and qmd loads from it. latchkey is left out — its
# `--version` initializes a keyring, which a bare runner has none of.
runtime/node/bin/node --version >&2
runtime/node/bin/node runtime/qmd/*/node_modules/@tobilu/qmd/dist/cli/qmd.js --version >&2

tarball="runtime-${triple}.tar.gz"
tar -czf "$tarball" -C runtime .
checksum "$tarball"
ls -la "$tarball" >&2
echo "$tarball"
echo "$tarball.sha256"
if [[ "$cuda" == "true" ]]; then
    cuda_tarball="runtime-${triple}-cuda.tar.gz"
    tar -czf "$cuda_tarball" -C runtime-cuda .
    checksum "$cuda_tarball"
    ls -la "$cuda_tarball" >&2
    echo "$cuda_tarball"
    echo "$cuda_tarball.sha256"
fi
