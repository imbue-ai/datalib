#!/usr/bin/env bash
# build-signed-app.sh's compile, verify and whole-run phases run third-party
# code, so each refuses to start with signing material in its environment:
# what catches a step moved into the wrong release.yml job, which only a tag
# would otherwise run. Every case stops before the script does any work.
# RELEASE_SHELL picks the bash; the macOS variant sets /bin/bash, the 3.2 the
# mac runner has.

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

script="$(rlocation _main/datalib/tauri/build-signed-app.sh)"
[[ -f "$script" ]] || { echo "ERROR: build-signed-app.sh not in runfiles at $script" >&2; exit 1; }
shell="${RELEASE_SHELL:-bash}"

for phase in compile verify all; do
    for name in CERTS_TAR_GZ APPLE_API_KEY_ID APPLE_API_ISSUER APPLE_API_KEY \
        APPLE_API_KEY_PATH APPLE_SIGNING_IDENTITY ACTIONS_ID_TOKEN_REQUEST_TOKEN; do
        code=0
        output="$(env "$name=x" "$shell" "$script" "$phase" 2>&1)" || code=$?
        if [[ "$code" != 1 || "$output" != *"$name is set"* ]]; then
            echo "FAIL: $phase with $name set: exit $code, output: $output" >&2
            exit 1
        fi
    done
done

code=0
output="$("$shell" "$script" nonsense 2>&1)" || code=$?
if [[ "$code" != 2 || "$output" != *usage* ]]; then
    echo "FAIL: an unknown phase: exit $code, output: $output" >&2
    exit 1
fi
echo "PASS"
