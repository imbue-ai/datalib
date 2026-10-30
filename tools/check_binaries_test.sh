#!/usr/bin/env bash
# Runs scripts/release/check_binaries.sh against stand-in binaries: text
# files carrying the bazel-out paths a real one would, and a `datalib-dag`
# that prints a version. Guards the two ways a release shipped wrong — the
# app's fastbuild binaries through v0.47.0 and the unstamped v0.7.0 — and
# the check passing because it found nothing to look at. RELEASE_SHELL
# picks the bash; the macOS variant sets /bin/bash, the 3.2 the mac
# runner has.

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

script="$(rlocation _main/scripts/release/check_binaries.sh)"
[[ -f "$script" ]] || { echo "ERROR: check_binaries.sh not in runfiles at $script" >&2; exit 1; }
shell="${RELEASE_SHELL:-bash}"

# A directory of the five checked binaries, each naming `$out_dir`, with
# `datalib-dag` reporting `$version`.
make_bins() {
    local dir="$1" out_dir="$2" version="$3" name
    rm -rf "$dir"
    mkdir -p "$dir"
    for name in datalib-step datalib-http datalib-applet datalib-doltlite; do
        printf 'junk\0bazel-out/%s/bin/third-party/doltlite/sqlite3.c\0junk' "$out_dir" > "$dir/$name"
    done
    printf '#!/bin/sh\n# bazel-out/%s/bin/third-party/doltlite/sqlite3.c\necho "datalib-dag %s"\n' \
        "$out_dir" "$version" > "$dir/datalib-dag"
    chmod +x "$dir/datalib-dag"
}

failures=0
expect() {
    local want="$1" label="$2" dir="$3" pattern="${4:-}" out status
    set +e
    out="$("$shell" "$script" v1.2.3 "$dir" 2>&1)"
    status=$?
    set -e
    if [[ "$want" == pass && $status -ne 0 ]] || [[ "$want" == fail && $status -eq 0 ]]; then
        echo "FAIL: $label: expected $want, exit $status" >&2
        printf '%s\n' "$out" >&2
        failures=$((failures + 1))
    elif [[ -n "$pattern" ]] && ! printf '%s\n' "$out" | grep -qE -- "$pattern"; then
        echo "FAIL: $label: output does not match /$pattern/" >&2
        printf '%s\n' "$out" >&2
        failures=$((failures + 1))
    else
        echo "ok: $label"
    fi
}

d="$TEST_TMPDIR/bins"

make_bins "$d" darwin_arm64-opt v1.2.3
expect pass "mac opt, stamped" "$d"

make_bins "$d" k8-opt-ST-0123abcd v1.2.3
expect pass "linux opt under a platform transition" "$d"

make_bins "$d" darwin_arm64-fastbuild v1.2.3
expect fail "fastbuild" "$d" 'datalib-step was not built -c opt'

make_bins "$d" darwin_arm64-dbg v1.2.3
expect fail "dbg" "$d" 'datalib-http was not built -c opt'

make_bins "$d" darwin_arm64-opt v1.2.3
printf 'bazel-out/darwin_arm64-opt/bin/a\0bazel-out/darwin_arm64-fastbuild/bin/b' > "$d/datalib-applet"
expect fail "one binary mixing opt and fastbuild" "$d" 'datalib-applet was not built -c opt'

make_bins "$d" darwin_arm64-opt v1.2.3
printf 'no fingerprint here' > "$d/datalib-step"
expect fail "a binary with no bazel-out path" "$d" 'datalib-step names no bazel-out directory'

make_bins "$d" darwin_arm64-opt v1.2.3
rm "$d/datalib-doltlite"
expect fail "a missing binary" "$d" 'datalib-doltlite is missing'

make_bins "$d" darwin_arm64-opt dev
expect fail "unstamped" "$d" "says 'datalib-dag dev'"

[[ $failures -eq 0 ]] || { echo "$failures case(s) failed" >&2; exit 1; }
