#!/usr/bin/env bash
# Runs release.yml's `runtime` job, minus the upload, the way the job
# runs it: scripts/release/stage_runtime_asset.sh from a directory that
# is not the repo, with a relative <dest> — v0.35.0's job was the first
# to pass one, and the script's smoke test could not find the node it
# had just staged. Then checks the asset it produced runs its node and
# holds the pinned latchkey. RELEASE_SHELL picks the bash the script runs under and RELEASE_RSYNC
# the rsync; the macOS variant sets what the mac runner has, /bin/bash (3.2)
# and /usr/bin/rsync (openrsync), which a laptop's Homebrew rsync would hide.

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
latchkey_pin="$(grep -E -m1 '^pub const LATCHKEY_VERSION:' "$tree/datalib/backend/runtime/src/node_runtime.rs" | sed -E 's/.*"([^"]+)".*/\1/')"
[[ -n "$latchkey_pin" ]] || { echo "ERROR: no LATCHKEY_VERSION pin found" >&2; exit 1; }

shell="${RELEASE_SHELL:-bash}"
echo ">>> running under $("$shell" -c 'echo "$BASH_VERSION"')"

case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) triple=aarch64-apple-darwin ;;
    Linux-x86_64) triple=x86_64-unknown-linux-gnu ;;
    Linux-aarch64) triple=aarch64-unknown-linux-gnu ;;
    *) echo "ERROR: no runtime triple for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

if [[ -n "${RELEASE_RSYNC:-}" ]]; then
    mkdir -p "$TEST_TMPDIR/tools"
    ln -s "$RELEASE_RSYNC" "$TEST_TMPDIR/tools/rsync"
    export PATH="$TEST_TMPDIR/tools:$PATH"
fi

work="$TEST_TMPDIR/work"
mkdir -p "$work"
cd "$work"

files="$(STAGE_RUNTIME_BAZEL_BIN="$tree" "$shell" "$script" "$triple")"
echo "$files"
[[ "$files" == "runtime-$triple.tar.gz"$'\n'"runtime-$triple.tar.gz.sha256" ]] \
    || { echo "ERROR: unexpected asset list" >&2; exit 1; }
[[ -f "runtime-$triple.tar.gz" && -f "runtime-$triple.tar.gz.sha256" ]] \
    || { echo "ERROR: the asset or its sidecar was not written" >&2; exit 1; }

# The asset unpacks into a directory and runs from there — the shape
# the resolver relies on.
mkdir unpacked
tar -xzf "runtime-$triple.tar.gz" -C unpacked
# Bazel's outputs are read-only, and the app's build signs these files in place.
read_only="$(find unpacked ! -type l ! -perm -u+w)"
[[ -z "$read_only" ]] || { echo "ERROR: read-only in the asset:"$'\n'"$read_only" >&2; exit 1; }
unpacked/node/bin/node --version || { echo "ERROR: the unpacked asset's node does not run" >&2; exit 1; }
package="unpacked/latchkey/$latchkey_pin/node_modules/latchkey/package.json"
[[ -f "$package" ]] || { echo "ERROR: no latchkey $latchkey_pin in the unpacked asset" >&2; ls unpacked/latchkey >&2; exit 1; }
got="$(unpacked/node/bin/node -p "require('./$package').version")"
[[ "$got" == "$latchkey_pin" ]] || { echo "ERROR: the asset's latchkey says '$got', not $latchkey_pin" >&2; exit 1; }
[[ ! -e unpacked/qmd ]] || { echo "ERROR: the asset still carries qmd" >&2; exit 1; }
