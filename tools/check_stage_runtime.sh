#!/usr/bin/env bash
# Runs release.yml's `runtime` job, minus the upload, the way the job
# runs it: scripts/release/stage_runtime_asset.sh from a directory that
# is not the repo, with a relative <dest> — v0.35.0's job was the first
# to pass one, and the script's smoke test could not find the node it
# had just staged. Then checks the asset it produced names the pinned
# qmd. RELEASE_SHELL picks the bash the script runs under; the macOS
# variant sets /bin/bash, the 3.2 the mac runner has.

# --- begin runfiles.bash initialization v3 ---
# Copy-pasted from the Bazel Bash runfiles library v3.
set -uo pipefail; set +e
f=bazel_tools/tools/bash/runfiles/runfiles.bash
# shellcheck disable=SC1090
source "${RUNFILES_DIR:-/dev/null}/$f" 2>/dev/null || \
  source "$(grep -sm1 "^$f " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f2- -d' ')" 2>/dev/null || \
  source "$0.runfiles/$f" 2>/dev/null || \
  source "$(grep -sm1 "^$f " "$0.runfiles_manifest" | cut -f2- -d' ')" 2>/dev/null || \
  { echo>&2 "ERROR: cannot find $f"; exit 1; }; f=; set -e
# --- end runfiles.bash initialization v3 ---

script="$(rlocation _main/scripts/release/stage_runtime_asset.sh)"
[[ -f "$script" ]] || { echo "ERROR: stage_runtime_asset.sh not in runfiles at $script" >&2; exit 1; }
# The runfiles tree lays the Bazel outputs out exactly as bazel-bin does.
tree="${script%/scripts/release/stage_runtime_asset.sh}"
qmd_pin="$(grep -E -m1 '^pub const DEFAULT_QMD_VERSION:' "$tree/datalib/backend/runtime/src/qmd.rs" | sed -E 's/.*"([^"]+)".*/\1/')"
[[ -n "$qmd_pin" ]] || { echo "ERROR: no DEFAULT_QMD_VERSION pin found" >&2; exit 1; }

shell="${RELEASE_SHELL:-bash}"
echo ">>> running under $("$shell" -c 'echo "$BASH_VERSION"')"

case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) triple=aarch64-apple-darwin ;;
    Linux-x86_64) triple=x86_64-unknown-linux-gnu ;;
    Linux-aarch64) triple=aarch64-unknown-linux-gnu ;;
    *) echo "ERROR: no runtime triple for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

work="$TEST_TMPDIR/work"
mkdir -p "$work"
cd "$work"

files="$(STAGE_RUNTIME_BAZEL_BIN="$tree" "$shell" "$script" "$triple" false)"
echo "$files"
[[ "$files" == "runtime-$triple.tar.gz"$'\n'"runtime-$triple.tar.gz.sha256" ]] \
    || { echo "ERROR: unexpected asset list" >&2; exit 1; }
[[ -f "runtime-$triple.tar.gz" && -f "runtime-$triple.tar.gz.sha256" ]] \
    || { echo "ERROR: the asset or its sidecar was not written" >&2; exit 1; }

# The asset unpacks into a directory and runs from there — the shape
# the resolver relies on.
mkdir unpacked
tar -xzf "runtime-$triple.tar.gz" -C unpacked
got="$(unpacked/node/bin/node unpacked/qmd/*/node_modules/@tobilu/qmd/dist/cli/qmd.js --version)"
echo "$got"
case "$got" in
    "qmd $qmd_pin"*) ;;
    *) echo "ERROR: expected 'qmd $qmd_pin …' from the unpacked asset, got '$got'" >&2; exit 1 ;;
esac
