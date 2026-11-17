#!/usr/bin/env bash
# The `runtime` job of release.yml, minus handing the files on: stage the Node
# runtime for this host into `runtime/`, prove its node runs, and tar it into
# the asset the binaries fetch on first use (docs/dev/runtime_fetch.md).
# Writes `runtime-<triple>.tar.gz` and its `.sha256` sidecar into the current
# directory and prints those file names, one per line.
#
#   scripts/release/stage_runtime_asset.sh <triple>
#
# A script rather than a `run:` block so the same lines run under
# //tools:stage_runtime_test — including on a mac's /bin/bash 3.2 —
# before a tag ever runs them. Inner scripts are run through "$BASH" so
# whichever bash runs this one runs them too.
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <triple>" >&2
    exit 2
fi
triple="$1"

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$script_dir/../.."

checksum() {
    if command -v sha256sum >/dev/null; then
        sha256sum "$1" > "$1.sha256"
    else
        shasum -a 256 "$1" > "$1.sha256"
    fi
}

"$BASH" "$repo_root/scripts/stage_runtime.sh" runtime

# Smoke the staged tree on this host (the same triple as the asset): the
# Node runs. latchkey is left out — its `--version` initializes a
# keyring, which a bare runner has none of; stage_runtime.sh has already
# proved it resolves playwright.
runtime/node/bin/node --version >&2

tarball="runtime-${triple}.tar.gz"
tar -czf "$tarball" -C runtime .
checksum "$tarball"
ls -la "$tarball" >&2
echo "$tarball"
echo "$tarball.sha256"
