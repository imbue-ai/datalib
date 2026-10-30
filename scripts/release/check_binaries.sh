#!/usr/bin/env bash
# Refuse to ship backend binaries that are not the release build: run by
# the `build` job on the tarball's binaries and by the `macos-app` job on
# the ones bundled in Datalib.app, before either publishes anything.
#
#   scripts/release/check_binaries.sh <ref-name> <dir>
#
# Two checks, each a way a release has shipped wrong before:
#
#   optimized  Bazel's default compilation mode, `fastbuild`, leaves the
#              Rust unoptimized and a sync about twice as slow; the app
#              shipped that way through v0.47.0. Bazel names its output
#              directory after the mode (`darwin_arm64-opt`, `k8-fastbuild`),
#              and that name is compiled into a binary through the paths
#              of generated sources (doltlite's `sqlite3.c`, a proto
#              crate). Each binary below must name at least one, and only
#              `-opt` ones; naming none means this check cannot tell, which
#              is a failure too.
#   stamped    `datalib-dag --version` is exactly `datalib-dag <ref-name>`.
#              An unstamped build says `dev` (v0.7.0, v0.8.0).
#
# Runs the `datalib-dag` it checks, so the caller is on the binaries' own
# platform. //tools:check_binaries_test runs it against stand-ins.
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $0 <ref-name> <dir>" >&2
    exit 2
fi
ref_name="$1"
dir="$2"

failed=0
complain() { printf 'check_binaries: error: %s\n' "$*" >&2; failed=1; }

# The binaries that link a generated source, so carry the fingerprint.
# `datalib-migrate-config` and the curl shims carry none.
for name in datalib-step datalib-http datalib-dag datalib-applet datalib-doltlite; do
    bin="$dir/$name"
    if [[ ! -f "$bin" ]]; then
        complain "$bin is missing"
        continue
    fi
    dirs="$(LC_ALL=C grep -aoE 'bazel-out/[^/]+/bin/' "$bin" | sort -u || true)"
    if [[ -z "$dirs" ]]; then
        complain "$name names no bazel-out directory, so its compilation mode cannot be told"
        continue
    fi
    not_opt="$(printf '%s\n' "$dirs" | grep -vE -- '-opt(-ST-[0-9a-f]+)?/bin/$' || true)"
    if [[ -n "$not_opt" ]]; then
        complain "$name was not built -c opt; it names $(printf '%s' "$not_opt" | tr '\n' ' ')"
        continue
    fi
    echo "$name: optimized ($(printf '%s' "$dirs" | tr '\n' ' '))"
done

expected="datalib-dag $ref_name"
if [[ -x "$dir/datalib-dag" ]]; then
    actual="$("$dir/datalib-dag" --version)"
    if [[ "$actual" == "$expected" ]]; then
        echo "datalib-dag --version: $actual"
    else
        complain "datalib-dag --version says '$actual', not '$expected'; the build was not stamped (--config=release)"
    fi
fi

exit "$failed"
