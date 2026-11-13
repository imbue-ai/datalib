#!/usr/bin/env bash
# Runs scripts/release/check_glibc_floor.sh against stand-in ELF files: the
# magic and the version names a dynamic string table holds. Guards v0.52.0's
# x86_64 tarball, built on Ubuntu 26.04, which needed GLIBC_2.43 and would not
# start on 24.04; and the check passing because it found nothing to read.
# RELEASE_SHELL picks the bash; the macOS variant sets /bin/bash.

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

script="$(rlocation _main/scripts/release/check_glibc_floor.sh)"
[[ -f "$script" ]] || { echo "ERROR: check_glibc_floor.sh not in runfiles at $script" >&2; exit 1; }
shell="${RELEASE_SHELL:-bash}"

# An ELF stand-in naming each of its arguments as a NUL-terminated string.
make_elf() {
    local file="$1" name
    shift
    mkdir -p "$(dirname "$file")"
    printf '\177ELF\002\001\001\0junk\0' > "$file"
    for name in "$@"; do
        printf '%s\0' "$name" >> "$file"
    done
}

failures=0
# pass|fail, the case, the output it must match ('' for any), then the arguments.
expect() {
    local want="$1" label="$2" pattern="$3" out status
    shift 3
    set +e
    out="$("$shell" "$script" "$@" 2>&1)"
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

n=0
fresh() { n=$((n + 1)); d="$TEST_TMPDIR/case$n"; mkdir -p "$d"; }

fresh
make_elf "$d/bin/datalib-step" libc.so.6 GLIBC_2.2.5 GLIBC_2.34 GLIBC_2.39
make_elf "$d/bin/lib/libdoltlite.so" GLIBC_2.17
printf '#!/bin/sh\necho GLIBC_2.43\n' > "$d/bin/latchkey"
expect pass "everything at or under the floor" '2 ELF files read, against GLIBC_2.39' "$d/bin"

fresh
make_elf "$d/bin/datalib-step" GLIBC_2.2.5 GLIBC_2.39 GLIBC_2.43
expect fail "above the floor (v0.52.0's x86_64 build)" \
    'bin:datalib-step needs GLIBC_2.43, above the floor GLIBC_2.39' "$d/bin"

fresh
make_elf "$d/runtime/node/bin/node" GLIBC_2.28
make_elf "$d/runtime/qmd/node_modules/x/build/binding.node" GLIBC_2.40
expect fail "a library deep in a tree" \
    'runtime:qmd/node_modules/x/build/binding.node needs GLIBC_2.40' "$d/runtime"

fresh
make_elf "$d/bin/datalib-step" GLIBC_3.1
expect fail "a major version above the floor" 'needs GLIBC_3.1' "$d/bin"

fresh
make_elf "$d/bin/datalib-step" 'atan2f@GLIBC_2.43' 'GLIBC_2.43 is new' GLIBC_PRIVATE GLIBC_2.39
expect pass "only a whole version-name string counts" '' "$d/bin"

fresh
make_elf "$d/bin/datalib-step"
expect pass "a static binary names no glibc version" '' "$d/bin"

fresh
make_elf "$d/stage/datalib-0.0.0-x/datalib-step" GLIBC_2.43
tar -czf "$d/datalib-x.tar.gz" -C "$d/stage" .
expect fail "inside a tarball" 'datalib-x.tar.gz:.*datalib-step needs GLIBC_2.43' "$d/datalib-x.tar.gz"

fresh
make_elf "$d/ok/datalib-step" GLIBC_2.39
make_elf "$d/bad/datalib-step" GLIBC_2.41
expect fail "every argument is read" 'bad:datalib-step needs GLIBC_2.41' "$d/ok" "$d/bad"

fresh
mkdir -p "$d/empty"
printf 'GLIBC_2.43\0' > "$d/empty/notes.txt"
expect fail "nothing to read" 'holds no ELF file' "$d/empty"

expect fail "a path that is not there" 'neither a directory nor a file' "$d/missing"

[[ $failures -eq 0 ]] || { echo "$failures case(s) failed" >&2; exit 1; }
