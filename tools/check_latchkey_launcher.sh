#!/usr/bin/env bash
# Runs scripts/latchkey-wrapper.sh beside made-up runtime trees, whose
# `node` only says which tree it is. Guards which tree the launcher
# picks: after an upgrade the cache still holds the previous release's
# runtime, which the launcher once ran without a word. And that a latchkey
# run by hand has its usage count off, as one datalib starts does.

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

wrapper="$(rlocation _main/scripts/latchkey-wrapper.sh)"
[[ -f "$wrapper" ]] || { echo "ERROR: latchkey-wrapper.sh not in runfiles at $wrapper" >&2; exit 1; }

work="$(mktemp -d "${TEST_TMPDIR:-/tmp}/launcher.XXXXXX")"
# The previous release's digest sorts first, so a lookup that takes the
# first tree it finds takes that one.
previous_sha=aaaaaaaaaaaa0000000000000000000000000000000000000000000000000000
this_sha=bbbbbbbbbbbb0000000000000000000000000000000000000000000000000000
previous_dir=aaaaaaaaaaaa
this_dir=bbbbbbbbbbbb

failed=0
complain() { echo "FAIL: $1" >&2; failed=1; }

make_runtime() { # directory, what its node says
    mkdir -p "$1/node/bin" "$1/latchkey/0.0.0/node_modules/latchkey/dist/src"
    printf '#!/bin/sh\necho "%s"\n' "$2" > "$1/node/bin/node"
    chmod +x "$1/node/bin/node"
    touch "$1/latchkey/0.0.0/node_modules/latchkey/dist/src/cli.js"
}

# A release directory holding the launcher, an empty home and an empty
# cache. Sets $release, $home and $cache.
fresh() { # name of the case
    release="$work/$1/release"
    home="$work/$1/home"
    cache="$work/$1/cache"
    mkdir -p "$release" "$home" "$cache"
    install -m 0755 "$wrapper" "$release/latchkey"
}

write_manifest() {
    {
        echo "# The Node runtime these binaries fetch on first use"
        echo "cpu runtime-x.tar.gz $this_sha 100 https://example.com/runtime-x.tar.gz"
        echo "cuda runtime-x-cuda.tar.gz $previous_sha 200 https://example.com/runtime-x-cuda.tar.gz"
    } > "$release/runtime.manifest"
}

launcher() { # the program to run, then any VAR=value for its environment
    local program="$1"
    shift
    env -u DATALIB_RUNTIME_DIR -u LATCHKEY_CURL -u XDG_CACHE_HOME HOME="$home" "$@" \
        "$program" --version 2>&1
}

expect_runs() { # what its node should say, the case, then any VAR=value
    local want="$1" what="$2" got
    shift 2
    got="$(launcher "$release/latchkey" "$@")" || complain "$what: exited non-zero: $got"
    [[ "$got" == "$want" ]] || complain "$what: said '$got', not '$want'"
}

expect_refuses() { # text its message should hold, the case, then any VAR=value
    local want="$1" what="$2" got
    shift 2
    if got="$(launcher "$release/latchkey" "$@")"; then
        complain "$what: ran, and said '$got'"
    elif [[ "$got" != *"$want"* ]]; then
        complain "$what: refused with '$got', which does not say '$want'"
    fi
}

fresh upgraded
write_manifest
make_runtime "$cache/datalib/runtime/$previous_dir" "previous release"
expect_refuses "datalib-step pull-runtime" "only the previous release's tree is cached" "XDG_CACHE_HOME=$cache"

fresh fetched
write_manifest
make_runtime "$cache/datalib/runtime/$previous_dir" "previous release"
make_runtime "$cache/datalib/runtime/$this_dir" "this release"
expect_runs "this release" "both trees are cached" "XDG_CACHE_HOME=$cache"

fresh home_cache
write_manifest
make_runtime "$home/.cache/datalib/runtime/$this_dir" "this release"
expect_runs "this release" "the cache under HOME, with no XDG_CACHE_HOME"
expect_runs "this release" "an empty XDG_CACHE_HOME counts as unset" "XDG_CACHE_HOME="

fresh linked
write_manifest
make_runtime "$cache/datalib/runtime/$this_dir" "this release"
mkdir -p "$work/linked/bin"
ln -s "$release/latchkey" "$work/linked/bin/latchkey"
got="$(launcher "$work/linked/bin/latchkey" "XDG_CACHE_HOME=$cache")" || complain "through a link: exited non-zero: $got"
[[ "$got" == "this release" ]] || complain "through a link: said '$got', which means the manifest was not read beside the real file"

fresh sibling
write_manifest
make_runtime "$cache/datalib/runtime/$this_dir" "this release, cached"
make_runtime "$release/runtime" "beside the binaries"
expect_runs "beside the binaries" "a runtime beside the binaries wins over the cache" "XDG_CACHE_HOME=$cache"

fresh one_level_up
write_manifest
make_runtime "$work/one_level_up/runtime" "one level up"
expect_runs "one level up" "a runtime one level up, as in the .app" "XDG_CACHE_HOME=$cache"

fresh override
write_manifest
make_runtime "$release/runtime" "beside the binaries"
make_runtime "$work/override/elsewhere" "the override"
expect_runs "the override" "DATALIB_RUNTIME_DIR wins" "DATALIB_RUNTIME_DIR=$work/override/elsewhere"

fresh no_manifest
make_runtime "$cache/datalib/runtime/$this_dir" "someone else's tree"
expect_refuses "stage_runtime.sh" "with no manifest nothing names a tree in the cache" "XDG_CACHE_HOME=$cache"

fresh counting
make_runtime "$release/runtime" "unused"
printf '#!/bin/sh\necho "counting off: ${LATCHKEY_DISABLE_COUNTING:-unset}"\n' > "$release/runtime/node/bin/node"
expect_runs "counting off: 1" "the launcher turns latchkey's usage count off"
expect_runs "counting off: 1" "whatever the environment said" "LATCHKEY_DISABLE_COUNTING=0"

exit "$failed"
